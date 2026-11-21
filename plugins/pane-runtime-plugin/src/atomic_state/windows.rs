use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileRenameInfoEx, FlushFileBuffers, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    MoveFileExW, SetFileInformationByHandle,
};

/// Replace an existing destination.
const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
/// Unlink the replaced file even while readers still hold it open.
const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;

pub(super) fn create(path: &Path) -> std::io::Result<std::fs::File> {
    // The caller's private state directory supplies the inherited Windows ACL.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Atomically replace `path` with `temporary`.
///
/// Readers may hold the previous state open (std opens files with delete
/// sharing). A classic `MoveFileExW` replacement fails with "Access is denied"
/// while they do, so rename with POSIX semantics, which unlinks the old file
/// and leaves open readers on their snapshot. File systems without POSIX
/// renames (FAT, some network shares) fall back to `MoveFileExW`.
pub(super) fn publish(temporary: &Path, path: &Path, _parent: &Path) -> std::io::Result<()> {
    match rename_posix(temporary, path) {
        Err(error)
            if [ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED]
                .iter()
                .any(|code| error.raw_os_error() == i32::try_from(*code).ok()) =>
        {
            move_file(temporary, path)
        }
        result => result,
    }
}

fn rename_posix(temporary: &Path, path: &Path) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .access_mode(DELETE | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(temporary)?;
    // Without a root directory handle the destination must be a full path.
    let name: Vec<u16> = std::path::absolute(path)?
        .as_os_str()
        .encode_wide()
        .collect();
    let name_bytes = name.len() * size_of::<u16>();
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = offset + name_bytes + size_of::<u16>();
    // u64 storage keeps the structure's pointer-sized fields aligned.
    let mut storage = vec![0_u64; size.div_ceil(size_of::<u64>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: storage is zeroed, aligned and large enough for the header and
    // the name plus a terminator; the name is copied within those bounds.
    unsafe {
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).FileNameLength = u32::try_from(name_bytes).map_err(std::io::Error::other)?;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    // SAFETY: an open handle with DELETE access and a valid rename buffer.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast(),
            u32::try_from(size).map_err(std::io::Error::other)?,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // The contents were flushed before publication; flush the rename too.
    // SAFETY: an open file handle.
    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn move_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // FlushFileBuffers already completed before publication. WRITE_THROUGH waits
    // for the replacement move; opening a directory and fsyncing it is Unix-only.
    // SAFETY: null-terminated paths remain live for the synchronous OS call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
