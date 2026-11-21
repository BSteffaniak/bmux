//! Verified executable identity: hash an exact executable file and retain
//! content-addressed private copies of it. No discovery or executable search.
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[path = "executable_identity/retained.rs"]
mod retained;

pub use retained::retain as retain_executable;

/// An anonymous copy-on-write snapshot of the opened file, never a hard link.
#[cfg(target_os = "macos")]
pub(super) fn try_clone_snapshot(input: &File) -> std::io::Result<File> {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let path = directory.path().join("snapshot");
    let file = retained::clone(input, &path)?;
    std::fs::remove_file(path)?;
    Ok(file)
}

/// Hash an exact executable while checking for replacement and modification.
///
/// # Errors
/// Rejects noncanonical paths, non-executable or non-regular files, and changes
/// observed while reading. The returned digest identifies bytes, not trust.
pub fn executable_sha256(path: &Path) -> Result<String, String> {
    executable_identity(path).map(|(_, digest)| digest)
}

fn executable_identity(path: &Path) -> Result<(File, String), String> {
    let (mut file, opened) = open_verified_executable(path)?;
    #[cfg(windows)]
    let identity = windows_file_identity(&file)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("executable checksum failed: {error}"))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|error| error.to_string())?;
    if after.len() != opened.len() || after.modified().ok() != opened.modified().ok() {
        return Err("executable modified during checksum".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if current.dev() != opened.dev()
            || current.ino() != opened.ino()
            || after.ctime() != opened.ctime()
            || after.ctime_nsec() != opened.ctime_nsec()
            || !current.is_file()
            || current.mode() & 0o111 == 0
        {
            return Err("executable changed during checksum".into());
        }
    }
    #[cfg(windows)]
    {
        let current = open_windows_regular(&path)?;
        if windows_file_identity(&current)? != identity
            || path.canonicalize().map_err(|error| error.to_string())? != path
        {
            return Err("executable changed identity during checksum".into());
        }
    }
    let mut artifact_sha256 = String::with_capacity(64);
    for byte in hash.finalize() {
        use std::fmt::Write as _;
        write!(&mut artifact_sha256, "{byte:02x}").map_err(|error| error.to_string())?;
    }
    Ok((file, artifact_sha256))
}

/// Validate the leaf and keep its verified read handle alive through checksum
/// and inventory validation. Windows sharing restrictions end only when the
/// caller drops the returned File; native calls borrow rather than own it.
fn open_verified_executable(path: &Path) -> Result<(File, std::fs::Metadata), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("executable unavailable: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("executable must be a regular file, not a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("executable is not executable".into());
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err("executable permission verification is unsupported on this platform".into());
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("executable canonicalization failed: {error}"))?;
    if canonical != path {
        return Err("executable changed identity during resolution".into());
    }
    #[cfg(not(windows))]
    let file = File::open(path).map_err(|error| format!("executable open failed: {error}"))?;
    #[cfg(windows)]
    let file = open_windows_regular(path)?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev()
            || opened.ino() != metadata.ino()
            || !opened.is_file()
            || opened.mode() & 0o111 == 0
        {
            return Err("executable replaced during open".into());
        }
    }
    Ok((file, opened))
}

#[cfg(windows)]
fn open_windows_regular(path: &Path) -> Result<File, String> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_READ, FILE_TYPE_DISK, GetFileType,
    };
    // No write/delete sharing: a verified artifact cannot be replaced or changed
    // through a newly opened handle while this read handle remains alive. Open
    // the leaf itself, never a reparse target (including non-symlink reparse tags).
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| format!("executable no-follow open failed: {error}"))?;
    let information = windows_file_information(&file)?;
    // SAFETY: the borrowed handle belongs to a live File for this call.
    if information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || unsafe { GetFileType(file.as_raw_handle().cast()) } != FILE_TYPE_DISK
    {
        return Err("executable must be a regular disk file, not a reparse point".into());
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_file_information(
    file: &File,
) -> Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION, String> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: writable initialized output and a borrowed live file handle.
    unsafe {
        let mut information: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(file.as_raw_handle().cast(), &raw mut information) == 0 {
            return Err(format!(
                "executable handle identity unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(information)
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Result<(u64, [u8; 16]), String> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    // SAFETY: FileIdInfo requires this initialized structure and exact size;
    // the file handle is borrowed for the call. The 128-bit ID also covers ReFS.
    unsafe {
        let mut information: FILE_ID_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileIdInfo,
            std::ptr::from_mut(&mut information).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>())
                .expect("file identity structure size fits u32"),
        ) == 0
        {
            return Err(format!(
                "executable file identity unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        if information.FileId.Identifier == [0; 16] {
            return Err("executable filesystem did not provide a stable file identity".into());
        }
        Ok((
            information.VolumeSerialNumber,
            information.FileId.Identifier,
        ))
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_verified_open_denies_writes_and_replacement() {
        let root = std::env::temp_dir().join(format!("bmux-windows-open-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("worker.exe");
        std::fs::write(&path, b"abc").unwrap();
        let file = open_windows_regular(&path).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::rename(&path, root.join("replacement.exe")).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert_eq!(
            windows_file_identity(&file).unwrap(),
            windows_file_identity(&open_windows_regular(&path).unwrap()).unwrap()
        );
        assert!(open_windows_regular(&root).is_err());
        drop(file);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn windows_verified_open_rejects_junctions_without_symlink_privileges() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        const SET_REPARSE_POINT: u32 = 0x0009_00a4;
        const MOUNT_POINT_TAG: u32 = 0xa000_0003;
        let root =
            std::env::temp_dir().join(format!("bmux-windows-reparse-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let link = root.join("junction");
        let target = root.join("target");
        std::fs::create_dir(&link).unwrap();
        std::fs::create_dir(&target).unwrap();
        let substitute: Vec<u16> = format!("\\??\\{}", target.display())
            .encode_utf16()
            .collect();
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&MOUNT_POINT_TAG.to_le_bytes());
        let data_bytes = u16::try_from(8 + (substitute.len() + 2) * 2).unwrap();
        buffer.extend_from_slice(&data_bytes.to_le_bytes());
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        buffer.extend_from_slice(&u16::try_from(substitute.len() * 2).unwrap().to_le_bytes());
        buffer.extend_from_slice(
            &u16::try_from((substitute.len() + 1) * 2)
                .unwrap()
                .to_le_bytes(),
        );
        buffer.extend_from_slice(&0_u16.to_le_bytes());
        for unit in substitute.into_iter().chain([0, 0]) {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&link)
            .unwrap();
        let mut returned = 0_u32;
        // SAFETY: handle belongs to the open directory, buffer is a complete
        // mount-point reparse structure, and no asynchronous IO is requested.
        unsafe {
            assert_ne!(
                DeviceIoControl(
                    file.as_raw_handle().cast(),
                    SET_REPARSE_POINT,
                    buffer.as_ptr().cast(),
                    u32::try_from(buffer.len()).unwrap(),
                    std::ptr::null_mut(),
                    0,
                    &raw mut returned,
                    std::ptr::null_mut()
                ),
                0
            );
        }
        drop(file);
        assert!(open_windows_regular(&link).is_err());
        std::fs::remove_dir(link).unwrap();
        std::fs::remove_dir(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
