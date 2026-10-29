//! Windows storage is rooted in retained NT directory handles, never joined paths.
//! Every component is opened without following reparse points. Records require
//! the current-user owner/private DACL and a single link before use.
use anyhow::{Context, Result, bail};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    LOCKFILE_EXCLUSIVE_LOCK, LockFileEx, OPEN_EXISTING,
};

#[path = "windows_security.rs"]
mod security;
use security::Security;

// NT native APIs supply the RootDirectory operation Win32 CreateFile lacks.
// Layouts are the documented UNICODE_STRING, OBJECT_ATTRIBUTES and IO_STATUS_BLOCK.
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
    fn NtSetInformationFile(
        handle: HANDLE,
        status: *mut IoStatus,
        information: *const core::ffi::c_void,
        length: u32,
        class: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
const READ_CONTROL: u32 = 0x0002_0000;
const DELETE: u32 = 0x0001_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OPEN_IF: u32 = 3;
const FILE_DIRECTORY_FILE: u32 = 1;
const FILE_WRITE_THROUGH: u32 = 2;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

fn nt_result(status: i32) -> io::Result<()> {
    if status < 0 {
        // SAFETY: pure conversion of a returned NTSTATUS.
        Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) }.cast_signed(),
        ))
    } else {
        Ok(())
    }
}

fn component(name: &std::ffi::OsStr) -> io::Result<Vec<u16>> {
    let wide: Vec<_> = name.encode_wide().collect();
    // NT relative names must be exactly one non-special component. Disallow ADS,
    // separators and Win32 aliases rather than interpreting caller strings.
    if wide.is_empty()
        || wide.len() > 255
        || wide.iter().any(|c| matches!(*c, 0 | 47 | 58 | 92))
        || name == "."
        || name == ".."
    {
        return Err(io::Error::other("invalid private record component"));
    }
    Ok(wide)
}
fn record_name(name: &CStr) -> io::Result<Vec<u16>> {
    component(std::ffi::OsStr::new(
        name.to_str().map_err(io::Error::other)?,
    ))
}

fn relative(
    parent: &File,
    name: &mut [u16],
    directory: bool,
    disposition: u32,
    writable: bool,
    security: Option<&Security>,
) -> io::Result<File> {
    let length = u16::try_from(name.len() * 2).map_err(io::Error::other)?;
    let mut unicode = UnicodeString {
        length,
        maximum_length: length,
        buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: u32::try_from(size_of::<ObjectAttributes>()).map_err(io::Error::other)?,
        root: parent.as_raw_handle(),
        name: &raw mut unicode,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE
        security: security.map_or(null_mut(), |s| s.descriptor),
        quality: null_mut(),
    };
    let mut raw = null_mut();
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    // SAFETY: all NT structures and UTF-16 storage remain live until this
    // synchronous operation completes. The directory handle is owned by caller.
    nt_result(unsafe {
        NtCreateFile(
            &raw mut raw,
            GENERIC_READ
                | READ_CONTROL
                | SYNCHRONIZE
                | if writable {
                    GENERIC_WRITE | if directory { 0 } else { DELETE }
                } else {
                    0
                },
            &raw const attributes,
            &raw mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | if directory { 0 } else { FILE_SHARE_DELETE },
            disposition,
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_WRITE_THROUGH
                | if directory {
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE
                },
            null(),
            0,
        )
    })?;
    // SAFETY: successful NtCreateFile returned a new exclusively owned handle.
    let file = unsafe { File::from_raw_handle(raw) };
    check_type(&file, directory)?;
    Ok(file)
}
fn check_type(file: &File, directory: bool) -> io::Result<()> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: file owns a valid handle and info is correctly sized output storage.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(io::Error::other(
            "private storage rejects reparse points, wrong types and linked records",
        ));
    }
    Ok(())
}
fn private(file: &File, directory: bool) -> Result<()> {
    check_type(file, directory)?;
    Security::current_user()?.check_file(file.as_raw_handle())?;
    Ok(())
}

pub struct PrivateDirectory {
    handle: File,
}
impl PrivateDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            bail!("absolute Windows storage root required");
        };
        if !matches!(
            prefix.kind(),
            std::path::Prefix::Disk(_)
                | std::path::Prefix::VerbatimDisk(_)
                | std::path::Prefix::UNC(_, _)
                | std::path::Prefix::VerbatimUNC(_, _)
        ) {
            bail!("storage root must name a filesystem volume, not a device namespace");
        }
        if components.next() != Some(Component::RootDir) {
            bail!("absolute storage root required");
        }
        let mut root = prefix.as_os_str().to_os_string();
        root.push("\\");
        let root = security::wide(root.as_os_str())?;
        // SAFETY: root is terminated UTF-16, null security/template, owned result.
        let raw = unsafe {
            CreateFileW(
                root.as_ptr(),
                GENERIC_READ | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error()).context("open storage volume root");
        }
        let mut handle = unsafe { File::from_raw_handle(raw) };
        check_type(&handle, true)?;
        let parts: Vec<_> = components.collect();
        if parts.is_empty() {
            bail!("private storage cannot be a volume root");
        }
        let security = Security::current_user()?;
        for (index, part) in parts.iter().enumerate() {
            let Component::Normal(name) = part else {
                bail!("non-normal storage path component");
            };
            let last = index + 1 == parts.len();
            handle = relative(
                &handle,
                &mut component(name)?,
                true,
                if last { FILE_OPEN_IF } else { FILE_OPEN },
                last,
                last.then_some(&security),
            )?;
        }
        private(&handle, true)?;
        Ok(Self { handle })
    }
    fn open_record(&self, name: &CStr, disposition: u32, writable: bool) -> Result<File> {
        private(&self.handle, true)?;
        let security = Security::current_user()?;
        let file = relative(
            &self.handle,
            &mut record_name(name)?,
            false,
            disposition,
            writable,
            Some(&security),
        )?;
        private(&file, false)?;
        Ok(file)
    }
    pub(crate) fn read(&self) -> Result<Option<Vec<u8>>> {
        self.read_record(c"identity.json", 16_384)
    }
    pub(super) fn read_record(&self, name: &CStr, limit: u64) -> Result<Option<Vec<u8>>> {
        let mut file = match self.open_record(name, FILE_OPEN, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len())? > limit {
            bail!("private record exceeds size limit");
        }
        Ok(Some(bytes))
    }
    pub(super) fn lock(&self, name: &CStr) -> Result<File> {
        let file = self.open_record(name, FILE_OPEN_IF, true)?;
        let mut overlapped = unsafe { std::mem::zeroed() };
        // SAFETY: synchronous file handle; lock byte zero. A distinct handle per
        // transaction serializes threads and processes, and close releases lock.
        if unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                1,
                0,
                &raw mut overlapped,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("lock pairing store");
        }
        Ok(file)
    }
    pub(super) fn remove_record(&self, name: &CStr) -> Result<()> {
        let file = self.open_record(name, FILE_OPEN, true)?;
        // POSIX disposition removes the name while this writable handle stays
        // alive, allowing its namespace change to be flushed before success.
        let flags: u32 = 3; // FILE_DISPOSITION_DELETE | POSIX_SEMANTICS
        let mut status = IoStatus {
            status: 0,
            information: 0,
        };
        nt_result(unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &raw mut status,
                (&raw const flags).cast(),
                4,
                64,
            )
        })?;
        file.sync_all()?;
        Ok(())
    }
    pub(crate) fn publish(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        let temporary = CString::new(format!(".identity-{}", uuid::Uuid::new_v4()))?;
        let mut file = self.open_record(&temporary, FILE_CREATE, true)?;
        let mut installed = false;
        let result: Result<Vec<u8>> = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            match rename(&file, &self.handle, c"identity.json", false) {
                Ok(()) => {
                    installed = true;
                    file.sync_all()?;
                    Ok(bytes.to_vec())
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.read()?.context("identity winner disappeared")
                }
                Err(error) => Err(error.into()),
            }
        })();
        // After rename, even a flush error is an indeterminate commit: never
        // dispose the destination or replay publication. Only discard our temp.
        if !installed {
            dispose(&file)?;
        }

        result
    }
    pub(super) fn replace_record(&self, name: &CStr, bytes: &[u8]) -> Result<()> {
        let temporary = CString::new(format!(".pairing-{}", uuid::Uuid::new_v4()))?;
        let mut file = self.open_record(&temporary, FILE_CREATE, true)?;
        let mut installed = false;
        let result: Result<()> = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            rename(&file, &self.handle, name, true)?;
            installed = true;
            file.sync_all()?;
            Ok(())
        })();
        if !installed {
            let _ = dispose(&file);
        }

        result
    }
}

fn dispose(file: &File) -> io::Result<()> {
    let delete: u8 = 1;
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    // SAFETY: FILE_DISPOSITION_INFORMATION is one BOOLEAN, on this exact handle.
    nt_result(unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &raw mut status,
            (&raw const delete).cast(),
            1,
            13,
        )
    })
}
fn rename(file: &File, parent: &File, name: &CStr, replace: bool) -> io::Result<()> {
    #[repr(C)]
    struct Rename {
        replace: u8,
        root: HANDLE,
        length: u32,
        name: [u16; 1],
    }
    let name = record_name(name)?;
    let offset = std::mem::offset_of!(Rename, name);
    let bytes = offset + name.len() * 2;
    let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<Rename>();
    // SAFETY: aligned allocation holds complete variable-length NT rename info;
    // the parent handle pins destination and name is one validated component.
    unsafe {
        (*info).replace = u8::from(replace);
        (*info).root = parent.as_raw_handle();
        (*info).length = u32::try_from(name.len() * 2).map_err(io::Error::other)?;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).name).cast(),
            name.len(),
        );
    }
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    nt_result(unsafe {
        NtSetInformationFile(
            file.as_raw_handle(),
            &raw mut status,
            info.cast(),
            u32::try_from(bytes).map_err(io::Error::other)?,
            10,
        )
    })
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
