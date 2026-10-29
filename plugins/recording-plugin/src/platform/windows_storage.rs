use super::windows_security::{Handle, Security, wide};
use std::{
    fs::File,
    io,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, MoveFileExW,
    },
};

fn operation_error(operation: &str, path: &Path, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

fn checked(path: &Path, directory: bool) -> io::Result<File> {
    checked_with_policy(path, directory, false)
}

fn checked_with_policy(path: &Path, directory: bool, parent: bool) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(GENERIC_READ | 0x0002_0000 /* READ_CONTROL */)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    let file = options
        .open(path)
        .map_err(|error| operation_error("opening object for ACL validation", path, &error))?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.is_dir() != directory
        || (!directory && !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "reparse point or object type mismatch at {}",
                path.display()
            ),
        ));
    }
    let security = Security::current_user()?;
    if parent {
        security
            .check_parent(file.as_raw_handle())
            .map_err(|error| operation_error("validating trusted parent ACL", path, &error))?;
    } else {
        security.check_file(file.as_raw_handle()).map_err(|error| {
            operation_error("validating strict private object ACL", path, &error)
        })?;
    }
    Ok(file)
}
fn create_raw(path: &Path) -> io::Result<File> {
    let security = Security::current_user()?;
    let attributes = security.attributes();
    let name = wide(path.as_os_str())?;
    // SAFETY: security descriptor and UTF-16 path live through CreateFileW; handle is exclusively owned.
    let handle = Handle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE | GENERIC_READ | 0x0002_0000,
            0,
            &raw const attributes,
            CREATE_NEW,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    })?;
    security.check_file(handle.0)?;
    let raw = handle.0;
    std::mem::forget(handle);
    Ok(unsafe { File::from_raw_handle(raw) })
}
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    // Windows does not support flushing directory handles. Private directory
    // identity is checked; publication uses write-through MoveFileExW instead.
    checked_with_policy(path, true, true).map(drop)
}
pub(crate) fn publish(source: &Path, target: &Path) -> io::Result<()> {
    let source = wide(source.as_os_str())?;
    let target = wide(target.as_os_str())?;
    // SAFETY: both terminated path buffers remain live for the synchronous call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    if !path.exists() {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("directory has no parent"))?;
        if !parent.exists() {
            private_directory(parent)?;
        }
        create_private_directory(path)?;
    }
    checked(path, true).map(drop)
}
pub(crate) fn create_private_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("private directory has no parent"))?;
    let _parent = checked_with_policy(parent, true, true)?;
    let temporary = parent.join(format!(".recording-dir-{}", uuid::Uuid::new_v4()));
    let security = Security::current_user()?;
    let attributes = security.attributes();
    let name = wide(temporary.as_os_str())?;
    // SAFETY: valid terminated path and security descriptor remain live through creation.
    if unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = publish_noreplace(&temporary, path).map_err(|error| {
        operation_error(
            "publishing private directory with write-through",
            path,
            &error,
        )
    });
    if result.is_err() {
        let _ = std::fs::remove_dir(&temporary);
    }
    result?;
    checked(path, true).map(drop)
}

/// Establish the namespace entry with a write-through move before returning an
/// open writer. Callers still must flush contents before acknowledging a write.
pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("private file has no parent"))?;
    let temporary = parent.join(format!(".recording-create-{}", uuid::Uuid::new_v4()));
    let file = create_raw(&temporary)?;
    file.sync_all()?;
    drop(file);
    let result = publish_noreplace(&temporary, path);
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    Security::current_user()?.check_file(file.as_raw_handle())?;
    Ok(file)
}

pub(crate) fn open_append(path: &Path) -> io::Result<File> {
    match create_new(path) {
        Ok(file) => drop(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let file = std::fs::OpenOptions::new()
        .append(true)
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let checked = checked(path, false)?;
    drop(checked);
    Security::current_user()?.check_file(file.as_raw_handle())?;
    Ok(file)
}

pub(crate) fn publish_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    let source = wide(source.as_os_str())?;
    let target = wide(target.as_os_str())?;
    // SAFETY: valid terminated path buffers; no REPLACE_EXISTING means destination collisions fail.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn validate_runtime_socket(socket: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(socket.is_absolute(), "runtime endpoint must be absolute");
    anyhow::ensure!(!socket.as_os_str().is_empty(), "runtime endpoint is empty");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_private_tree_below_inherited_tempfile_parent() {
        let parent = tempfile::tempdir().expect("create inherited tempfile parent");
        // Match ConfigPaths: the generic data directory is not a recording leaf.
        let data = parent.path().join("data");
        std::fs::create_dir(&data).expect("create generic inherited data directory");
        checked_with_policy(&data, true, true)
            .unwrap_or_else(|error| panic!("safe inherited data parent rejected: {error}"));
        let leaf = data.join("recording-process-v4/deep/leaf");
        private_directory(&leaf)
            .unwrap_or_else(|error| panic!("fresh private tree creation failed: {error}"));
        checked(&leaf, true).expect("new recording leaf must remain strictly owner-private");
        checked(&data.join("recording-process-v4"), true)
            .expect("new recording root must remain strictly owner-private");
        crate::durable::write_atomic(&leaf.join("state.json"), b"first").unwrap();
        crate::durable::write_atomic(&leaf.join("state.json"), b"second").unwrap();
        assert_eq!(std::fs::read(leaf.join("state.json")).unwrap(), b"second");
    }

    #[test]
    fn validation_error_identifies_parent_policy_and_original_cause() {
        let parent = tempfile::tempdir().unwrap();
        let file = parent.path().join("not-directory");
        std::fs::write(&file, b"data").unwrap();
        let error = create_private_directory(&file.join("leaf")).unwrap_err();
        assert!(error.to_string().contains("object type mismatch"));
        assert_eq!(std::fs::read(&file).unwrap(), b"data");
    }
}
