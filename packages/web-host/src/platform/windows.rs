//! Windows retained-handle asset access and owner-private resolver cache.
//! Components are opened relative to directory handles, never followed through
//! reparse points; retained directory handles prohibit rename/delete races.
#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::Command;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, LOCKFILE_EXCLUSIVE_LOCK,
    LockFileEx, OPEN_EXISTING,
};
#[path = "windows_security.rs"]
mod security;
use security::{Handle, Security, wide};

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
const READ_CONTROL: u32 = 0x0002_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;

fn check(file: &File, directory: bool) -> io::Result<()> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: live file handle and correctly sized output buffer.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(io::Error::other(
            "web assets/cache reject reparse points and wrong file types",
        ));
    }
    Ok(())
}
fn child(
    parent: &File,
    name: &std::ffi::OsStr,
    directory: bool,
    disposition: u32,
    write: bool,
    security: Option<&Security>,
) -> io::Result<File> {
    let mut name: Vec<u16> = name.encode_wide().collect();
    if name.is_empty() || name.iter().any(|c| [0, 47, 58, 92].contains(c)) {
        return Err(io::Error::other("invalid web asset/cache component"));
    }
    let length = u16::try_from(name.len() * 2).map_err(io::Error::other)?;
    let mut name = UnicodeString {
        length,
        maximum_length: length,
        buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: parent.as_raw_handle(),
        name: &mut name,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE
        security: security.map_or(null_mut(), |security| security.descriptor),
        quality: null_mut(),
    };
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let mut raw = null_mut();
    // SAFETY: NT structures and UTF-16 buffer remain alive through synchronous
    // NtCreateFile. OPEN_REPARSE_POINT prevents following even the final entry.
    let result = unsafe {
        NtCreateFile(
            &mut raw,
            GENERIC_READ | READ_CONTROL | SYNCHRONIZE | if write { GENERIC_WRITE } else { 0 },
            &attributes,
            &mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            disposition,
            0x0020_0000 | 0x20 | if directory { 1 } else { 0x40 },
            null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) } as i32,
        ));
    }
    // SAFETY: successful NT open transferred unique ownership.
    let file = unsafe { File::from_raw_handle(raw) };
    check(&file, directory)?;
    Ok(file)
}

// Keep each ancestor alive while a caller uses the selected directory. Without
// this, an attacker could rename a validated ancestor and redirect later opens.
pub(crate) struct Directory {
    handles: Vec<File>,
}
impl Directory {
    fn last(&self) -> &File {
        self.handles.last().expect("root directory retained")
    }
}
fn root(path: &Path) -> io::Result<(Directory, Vec<std::ffi::OsString>)> {
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => {
                return Err(io::Error::other(
                    "web paths require an absolute local drive (no UNC/device paths)",
                ));
            }
        },
        _ => {
            return Err(io::Error::other(
                "web paths require an absolute local drive",
            ));
        }
    };
    if components.next() != Some(Component::RootDir) {
        return Err(io::Error::other("web path is not absolute"));
    }
    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            _ => return Err(io::Error::other("invalid web path component")),
        }
    }
    let root = wide(std::ffi::OsStr::new(&format!(
        r"\\?\{}:\",
        char::from(drive)
    )))?;
    // SAFETY: NUL-terminated root path; owned result adopted exactly once.
    let handle = Handle::new(unsafe {
        CreateFileW(
            root.as_ptr(),
            GENERIC_READ | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    })?;
    let file = unsafe { File::from_raw_handle(handle.0) };
    std::mem::forget(handle);
    check(&file, true)?;
    Ok((
        Directory {
            handles: vec![file],
        },
        names,
    ))
}
pub(crate) fn open_directory(path: &Path) -> io::Result<Directory> {
    let (mut directory, names) = root(path)?;
    for name in names {
        directory
            .handles
            .push(child(directory.last(), &name, true, 1, false, None)?);
    }
    Ok(directory)
}
pub(crate) fn open_asset(parent: &Directory, name: &str) -> io::Result<File> {
    child(
        parent.last(),
        std::ffi::OsStr::new(name),
        false,
        1,
        false,
        None,
    )
}
fn parent(path: &Path) -> io::Result<(Directory, &std::ffi::OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing web file name"))?;
    let directory = open_directory(
        path.parent()
            .ok_or_else(|| io::Error::other("missing web parent"))?,
    )?;
    Ok((directory, name))
}
pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    let (directory, name) = parent(path)?;
    child(directory.last(), name, false, 1, false, None)
}
pub(crate) fn new_file(path: &Path) -> io::Result<File> {
    let (directory, name) = parent(path)?;
    let security = Security::current_user()?;
    child(directory.last(), name, false, 2, true, Some(&security))
}
pub(crate) fn is_directory(path: &Path) -> io::Result<bool> {
    match open_directory(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    let (mut directory, names) = root(path)?;
    let security = Security::current_user()?;
    if names.is_empty() {
        return Err(io::Error::other("web cache cannot be a drive root"));
    }
    for (index, name) in names.iter().enumerate() {
        let file = match child(directory.last(), name, true, 1, false, None) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                child(directory.last(), name, true, 2, true, Some(&security))?
            }
            Err(error) => return Err(error),
        };
        // Existing ancestors may have ordinary inherited Windows ACLs; the
        // selected cache itself must be current-user only before we execute tools.
        if index + 1 == names.len() {
            security.check_file(file.as_raw_handle())?;
        }
        directory.handles.push(file);
    }
    Ok(())
}
pub(crate) struct Lock {
    _file: File,
    _directory: Directory,
}
impl Lock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let (directory, name) = parent(path)?;
        let security = Security::current_user()?;
        security.check_file(directory.last().as_raw_handle())?;
        let file = child(directory.last(), name, false, 3, true, Some(&security))?;
        security.check_file(file.as_raw_handle())?;
        let mut overlapped = unsafe { std::mem::zeroed() };
        // SAFETY: synchronous lock on an owned regular file. Closing releases the
        // lock; keep the handle in this owner until the cache transaction ends.
        if unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                1,
                0,
                &mut overlapped,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _file: file,
            _directory: directory,
        })
    }
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    // Windows does not support FlushFileBuffers on read-only directories. Files
    // are flushed individually; publication uses a write-through directory move.
    let _directory = open_directory(path)?;
    Ok(())
}
pub(crate) fn default_cache_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join("bmux/cache"))
}
pub(crate) fn source_command(_root: &Path, _args: &[&str]) -> io::Result<Command> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native Windows source web-asset generation requires a pinned native setup driver; use a verified release bundle or explicit --web-assets (Nix/Bash source driver is Unix-only)",
    ))
}

pub(crate) fn sync_regular(path: &Path) -> io::Result<()> {
    let (directory, name) = parent(path)?;
    child(directory.last(), name, false, 1, true, None)?.sync_all()
}
pub(crate) fn publish(stage: &Path, destination: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    // Retain every ancestor during the path-based Win32 move; the stage itself
    // must not be held without FILE_SHARE_DELETE because it is being renamed.
    let (_source_parent, _) = parent(stage)?;
    let (_destination_parent, _) = parent(destination)?;
    let source = wide(stage.as_os_str())?;
    let destination = wide(destination.as_os_str())?;
    // No REPLACE_EXISTING: concurrent or preexisting state remains an error.
    // SAFETY: both strings remain NUL terminated and live through the call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
