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
    EqualSid, GetAce, GetAclInformation, GetTokenInformation, IsWellKnownSid,
    OWNER_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
    WinLocalSystemSid,
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

    pub(super) fn attributes(&self) -> windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES size fits u32"),
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }

    /// Reject a substituted/public recording object even if its contents authenticate.
    pub(super) fn check_file(&self, handle: HANDLE) -> io::Result<()> {
        self.check_acl(handle, false)
    }

    /// Parent paths may be readable by others but never writable by an
    /// unprivileged identity (the Windows equivalent of Unix mode & 022 == 0).
    pub(super) fn check_parent(&self, handle: HANDLE) -> io::Result<()> {
        self.check_acl(handle, true)
    }

    fn check_acl(&self, handle: HANDLE, parent: bool) -> io::Result<()> {
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
            return Err(io::Error::from_raw_os_error(
                i32::try_from(error).map_err(io::Error::other)?,
            ));
        }
        let result = (|| {
            let sid = unsafe { (*self.token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
            if owner.is_null() || dacl.is_null() {
                return Err(io::Error::other(
                    "recording object has missing owner or unrestricted DACL",
                ));
            }
            let own = unsafe { EqualSid(owner, sid) } != 0;
            let privileged_owner = parent
                && (unsafe { IsWellKnownSid(owner, WinLocalSystemSid) } != 0
                    || unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0);
            if !own && !privileged_owner {
                return Err(io::Error::other("recording object owner is not trusted"));
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
                // Inherit-only ACEs do not authorize access to this object;
                // newly created children receive an explicit protected DACL.
                if unsafe { (*ace).Header.AceFlags } & 0x08 != 0 {
                    continue;
                }
                if unsafe { (*ace).Header.AceType } != 0
                /* ACCESS_ALLOWED_ACE_TYPE */
                {
                    return Err(io::Error::other("unsupported recording ACL entry"));
                }
                let trustee = unsafe { std::ptr::addr_of!((*ace).SidStart).cast_mut().cast() };
                let own = unsafe { EqualSid(trustee, sid) } != 0;
                // SYSTEM and administrators already have privilege to take
                // ownership, like root on Unix; this does not admit ordinary users.
                let privileged = parent
                    && (unsafe { IsWellKnownSid(trustee, WinLocalSystemSid) } != 0
                        || unsafe { IsWellKnownSid(trustee, WinBuiltinAdministratorsSid) } != 0);
                let readonly = parent
                    && unsafe { (*ace).Mask }
                        & !(windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                            | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE)
                        == 0;
                if !own && !privileged && !readonly {
                    return Err(io::Error::other("recording ACL grants untrusted access"));
                }
            }
            Ok(())
        })();
        unsafe { LocalFree(descriptor) };
        result
    }
}
