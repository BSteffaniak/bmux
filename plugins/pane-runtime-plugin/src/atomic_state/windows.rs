use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

pub(super) fn create(path: &Path) -> std::io::Result<std::fs::File> {
    // The caller's private state directory supplies the inherited Windows ACL.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}
pub(super) fn publish(temporary: &Path, path: &Path, _parent: &Path) -> std::io::Result<()> {
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
