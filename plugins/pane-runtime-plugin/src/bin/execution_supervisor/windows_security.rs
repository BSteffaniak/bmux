//! Windows resources: stable owned handles and owner-only security descriptors.
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
    EqualSid, GetAce, GetAclInformation, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub(super) struct Handle(pub HANDLE);
// SAFETY: owned kernel handles may be transferred between threads; this wrapper
// never duplicates ownership and APIs synchronize access to the underlying object.
unsafe impl Send for Handle {}
impl Handle {
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
    descriptor: *mut core::ffi::c_void,
    token_user: Vec<usize>,
}
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
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle::new(raw)?;
        let mut size = 0;
        // SAFETY: first call only obtains required size.
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut size) };
        // usize backing gives TOKEN_USER/SID their required alignment.
        let mut token_user = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        if size == 0
            || unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    token_user.as_mut_ptr().cast(),
                    size,
                    &mut size,
                )
            } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful query initialized a TOKEN_USER followed by its SID.
        let sid = unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut sid_text = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut sid_text) } == 0 {
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
            "O:{text}D:P(A;;GA;;;{text})"
        )))?;
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
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

    pub(super) fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }

    /// Reject a substituted/public bootstrap even if its contents authenticate.
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
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let result = (|| {
            let sid = unsafe { (*self.token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
            if owner.is_null() || dacl.is_null() || unsafe { EqualSid(owner, sid) } == 0 {
                return Err(io::Error::other("bootstrap owner or private ACL mismatch"));
            }
            let mut information: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                GetAclInformation(
                    dacl,
                    std::ptr::from_mut(&mut information).cast(),
                    size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            for index in 0..information.AceCount {
                let mut ace = null_mut();
                if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
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
                    return Err(io::Error::other("bootstrap ACL is not owner-private"));
                }
            }
            Ok(())
        })();
        unsafe { LocalFree(descriptor) };
        result
    }
}
