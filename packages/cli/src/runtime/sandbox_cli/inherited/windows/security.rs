//! Current-user owner and private DACL construction/checking for inherited sandboxes.
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub(super) struct Handle(pub HANDLE);
// SAFETY: owned kernel handles may be transferred between threads; this wrapper
// never duplicates ownership and APIs synchronize access to the underlying object.
unsafe impl Send for Handle {}
impl Handle {
    pub(super) const fn into_raw(self) -> HANDLE {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }

    pub(super) fn new(raw: HANDLE) -> io::Result<Self> {
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(raw))
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns this valid handle.
        unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn wide(value: &std::ffi::OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::other("embedded NUL in Windows argument"));
    }
    value.push(0);
    Ok(value)
}

pub(super) struct Security {
    pub(super) descriptor: *mut core::ffi::c_void,
    token_user: Vec<usize>,
}
// SAFETY: the descriptor allocation and token buffer are owned, immutable after
// construction, and only queried by read-only security APIs. Drop has exclusive
// ownership; moving or sharing this value never changes the pointed-to storage.
unsafe impl Send for Security {}
unsafe impl Sync for Security {}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: ConvertStringSecurityDescriptor allocates via LocalAlloc.
        unsafe { LocalFree(self.descriptor) };
    }
}
impl Security {
    pub(super) fn current_user() -> io::Result<Self> {
        let mut raw = null_mut();
        // SAFETY: valid output pointer and current process pseudo-handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle::new(raw)?;
        let mut size = 0;
        // SAFETY: first call only obtains required size.
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &raw mut size) };
        // usize backing gives TOKEN_USER/SID their required alignment.
        let mut token_user = vec![
            0usize;
            usize::try_from(size)
                .map_err(io::Error::other)?
                .div_ceil(size_of::<usize>())
        ];
        if size == 0
            || unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    token_user.as_mut_ptr().cast(),
                    size,
                    &raw mut size,
                )
            } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful query initialized a TOKEN_USER followed by its SID.
        let sid = unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut sid_text = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &raw mut sid_text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        // SAFETY: the API returns a NUL-terminated UTF-16 string.
        while unsafe { *sid_text.add(len) } != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_text, len) });
        unsafe { LocalFree(sid_text.cast()) };
        let sddl = wide(std::ffi::OsStr::new(&format!(
            "O:{text}D:P(A;OICI;GA;;;{text})"
        )))?;
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &raw mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor,
            token_user,
        })
    }

    /// Reject a substituted/public sandbox even if its contents authenticate.
    pub(super) fn check_file(&self, handle: HANDLE) -> io::Result<()> {
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: handle is open with READ_CONTROL; all outputs are valid pointers.
        let error = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &raw mut owner,
                null_mut(),
                &raw mut dacl,
                null_mut(),
                &raw mut descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(i32::from_ne_bytes(
                error.to_ne_bytes(),
            )));
        }
        let result = (|| {
            let mut control = 0;
            let mut revision = 0;
            // SAFETY: GetSecurityInfo returned a live security descriptor.
            if unsafe {
                GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if control & SE_DACL_PROTECTED == 0 {
                return Err(io::Error::other("sandbox DACL must be protected"));
            }
            let sid = unsafe { (*self.token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
            if owner.is_null() || dacl.is_null() || unsafe { EqualSid(owner, sid) } == 0 {
                return Err(io::Error::other("sandbox owner or private ACL mismatch"));
            }
            let mut information: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                GetAclInformation(
                    dacl,
                    std::ptr::from_mut(&mut information).cast(),
                    u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(io::Error::other)?,
                    AclSizeInformation,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            for index in 0..information.AceCount {
                let mut ace = null_mut();
                if unsafe { GetAce(dacl, index, &raw mut ace) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
                // Only a simple, current-user allow ACL is accepted. This is
                // intentionally fail-closed, including broad inherited ACLs.
                if unsafe { (*ace).Header.AceType } != 0 /* ACCESS_ALLOWED_ACE_TYPE */
                    || unsafe {
                        EqualSid(std::ptr::addr_of!((*ace).SidStart).cast_mut().cast(), sid)
                    } == 0
                {
                    return Err(io::Error::other("sandbox ACL is not owner-private"));
                }
            }
            Ok(())
        })();
        unsafe { LocalFree(descriptor) };
        result
    }
}
