//! Private, retained Windows sandbox directories. Never repair an existing ACL.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
};
mod security;
use security::Security;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: *mut core::ffi::c_void,
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
        handle: *mut *mut core::ffi::c_void,
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
    // SAFETY: live handle and correctly sized output.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(io::Error::other(
            "sandbox rejects reparse points and wrong file types",
        ));
    }
    Ok(())
}

fn child(
    parent: &File,
    name: &OsStr,
    directory: bool,
    create: bool,
    security: &Security,
) -> io::Result<File> {
    let mut name: Vec<u16> = name.encode_wide().collect();
    if name.is_empty() || name.iter().any(|c| [0, 47, 58, 92].contains(c)) {
        return Err(io::Error::other("invalid sandbox path component"));
    }
    let length = u16::try_from(name.len() * 2).map_err(io::Error::other)?;
    let mut name = UnicodeString {
        length,
        maximum_length: length,
        buffer: name.as_mut_ptr(),
    };
    let attributes = ObjectAttributes {
        length: u32::try_from(size_of::<ObjectAttributes>()).map_err(io::Error::other)?,
        root: parent.as_raw_handle(),
        name: &raw mut name,
        attributes: 0x40,
        security: if create {
            security.descriptor
        } else {
            null_mut()
        },
        quality: null_mut(),
    };
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let mut raw = null_mut();
    // SAFETY: synchronous native call; buffers live throughout. Relative opens,
    // OPEN_REPARSE_POINT and no DELETE sharing prevent path redirection.
    let result = unsafe {
        NtCreateFile(
            &raw mut raw,
            GENERIC_READ | READ_CONTROL | SYNCHRONIZE | if create { GENERIC_WRITE } else { 0 },
            &raw const attributes,
            &raw mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            if create { 2 } else { 1 },
            0x0020_0000 | 0x20 | if directory { 1 } else { 0x40 },
            null(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(i32::from_ne_bytes(
            unsafe { RtlNtStatusToDosError(result) }.to_ne_bytes(),
        )));
    }
    // SAFETY: successful open transfers unique ownership.
    let file = unsafe { File::from_raw_handle(raw) };
    check(&file, directory)?;
    if create {
        security.check_file(file.as_raw_handle())?;
    }
    Ok(file)
}

/// Keeps all ancestors locked against rename, including the non-private parent.
/// Public ancestors are allowed, but never modified; every owned child is private.
pub(super) struct Tree {
    handles: Vec<File>,
    root: PathBuf,
    root_index: usize,
    security: Security,
}
impl Tree {
    pub(super) fn create(path: &Path) -> io::Result<Self> {
        Self::open(path, true)
    }
    #[cfg(test)]
    pub(super) fn existing(path: &Path) -> io::Result<Self> {
        Self::open(path, false)
    }

    fn open(path: &Path, create: bool) -> io::Result<Self> {
        let security = Security::current_user()?;
        let mut components = path.components();
        let drive = match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
                _ => return Err(io::Error::other("sandbox requires local-drive paths")),
            },
            _ => return Err(io::Error::other("sandbox requires absolute paths")),
        };
        if components.next() != Some(Component::RootDir) {
            return Err(io::Error::other("sandbox requires absolute paths"));
        }
        let names: Vec<OsString> = components
            .map(|c| match c {
                Component::Normal(name) => Ok(name.to_owned()),
                _ => Err(io::Error::other("invalid sandbox path")),
            })
            .collect::<io::Result<_>>()?;
        if names.is_empty() {
            return Err(io::Error::other("sandbox cannot be drive root"));
        }
        let root: Vec<u16> = format!("{}:\\", char::from(drive))
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: NUL-terminated drive root; no creation and no DELETE sharing.
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
        let handle = security::Handle::new(raw)?;
        // SAFETY: transfer the sole owned handle to File.
        let file = unsafe { File::from_raw_handle(handle.into_raw()) };
        check(&file, true)?;
        let mut handles = vec![file];
        for (index, name) in names.iter().enumerate() {
            let file = child(
                handles.last().expect("retained root"),
                name,
                true,
                create && index == names.len() - 1,
                &security,
            )?;
            handles.push(file);
        }
        security.check_file(handles.last().expect("retained root").as_raw_handle())?;
        let root_index = handles.len() - 1;
        Ok(Self {
            handles,
            root: path.to_owned(),
            root_index,
            security,
        })
    }

    fn open_parent(&self, path: &Path) -> io::Result<Vec<File>> {
        let relative = path.strip_prefix(&self.root).map_err(io::Error::other)?;
        let mut parents = Vec::new();
        for part in relative.components() {
            let Component::Normal(name) = part else {
                return Err(io::Error::other("invalid sandbox child"));
            };
            let file = child(
                parents.last().unwrap_or(&self.handles[self.root_index]),
                name,
                true,
                false,
                &self.security,
            )?;
            self.security.check_file(file.as_raw_handle())?;
            parents.push(file);
        }
        Ok(parents)
    }

    pub(super) fn directory(&mut self, path: &Path) -> io::Result<()> {
        let parents =
            self.open_parent(path.parent().ok_or_else(|| io::Error::other("no parent"))?)?;
        let file = child(
            parents.last().unwrap_or(&self.handles[self.root_index]),
            path.file_name()
                .ok_or_else(|| io::Error::other("no name"))?,
            true,
            true,
            &self.security,
        )?;
        self.handles.extend(parents);
        self.handles.push(file);
        Ok(())
    }

    pub(super) fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let parents =
            self.open_parent(path.parent().ok_or_else(|| io::Error::other("no parent"))?)?;
        // The original sandbox root is at its known path, not necessarily the
        // last retained subdirectory. open_parent always starts from that handle.
        let mut file = child(
            parents.last().unwrap_or(&self.handles[self.root_index]),
            path.file_name()
                .ok_or_else(|| io::Error::other("no name"))?,
            false,
            true,
            &self.security,
        )?;
        file.write_all(bytes)?;
        file.sync_all()
    }
}

#[cfg(test)]
mod tests;
