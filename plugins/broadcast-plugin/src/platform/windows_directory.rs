//! Directory creation relative to a validated, retained parent handle.
use super::windows_security::Security;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use windows_sys::Win32::Foundation::HANDLE;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: HANDLE,
    name: *mut UnicodeString,
    attributes: u32,
    security: *mut core::ffi::c_void,
    quality: *mut core::ffi::c_void,
}
#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut HANDLE,
        access: u32,
        attributes: *const ObjectAttributes,
        status: *mut IoStatus,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const core::ffi::c_void,
        ea_length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

pub(super) fn open_or_create(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    let mut name: Vec<_> = name.encode_wide().collect();
    if name.is_empty()
        || name.len() > 255
        || name.iter().any(|c| matches!(*c, 0 | 47 | 58 | 92))
        || name == [46]
        || name == [46, 46]
    {
        return Err(io::Error::other(
            "private directory requires a single normal component",
        ));
    }
    let bytes = u16::try_from(name.len() * 2).map_err(io::Error::other)?;
    let mut unicode = UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: name.as_mut_ptr(),
    };
    let security = Security::current_user()?;
    let attributes = security.attributes();
    let object = ObjectAttributes {
        length: u32::try_from(size_of::<ObjectAttributes>()).map_err(io::Error::other)?,
        root: parent.as_raw_handle(),
        name: &raw mut unicode,
        attributes: 0x40,
        security: attributes.lpSecurityDescriptor,
        quality: std::ptr::null_mut(),
    };
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    // SAFETY: native structures and descriptor live through synchronous open;
    // parent supplies checked directory authority. FILE_OPEN_IF does not change
    // an existing ACL. FILE_OPEN_REPARSE_POINT refuses to follow the leaf.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            0x8012_0000,
            &object,
            &mut status,
            std::ptr::null(),
            0x80,
            3,
            3,
            0x0020_0021,
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    // SAFETY: successful NtCreateFile transferred exclusive handle ownership.
    let file = unsafe { File::from_raw_handle(handle) };
    use std::os::windows::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other(
            "private directory leaf is a reparse point or wrong type",
        ));
    }
    security.check_file(file.as_raw_handle())?;
    Ok(file)
}
