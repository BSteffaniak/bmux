//! Filesystem publication primitives. Callers own transaction locking and format validation.
use std::io::{self, Write as _};
use std::path::Path;

/// Publish a complete, flushed file without replacing existing state.
///
/// # Errors
/// Returns filesystem errors, including an existing destination.
pub fn publish_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    let mut stage = tempfile::NamedTempFile::new_in(parent)?;
    stage.write_all(bytes)?;
    stage.as_file().sync_all()?;
    #[cfg(windows)]
    {
        // Close the staging handle before moving it on Windows.
        let stage = stage.into_temp_path();
        rename_new(&stage, path)
    }
    #[cfg(not(windows))]
    {
        stage.persist_noclobber(path).map_err(|error| error.error)?;
        sync_parent(path)
    }
}

/// Atomically replace a value after flushing its complete contents.
///
/// # Errors
/// Returns staging, flush, or publication errors without accepting partial state.
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    let mut stage = tempfile::NamedTempFile::new_in(parent)?;
    stage.write_all(bytes)?;
    stage.as_file().sync_all()?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
        let stage = stage.into_temp_path();
        windows_move(&stage, path, MOVEFILE_REPLACE_EXISTING)
    }
    #[cfg(not(windows))]
    {
        stage.persist(path).map_err(|error| error.error)?;
        sync_parent(path)
    }
}

#[cfg(windows)]
fn windows_move(source: &Path, destination: &Path, flags: u32) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        value.push(0);
        Ok(value)
    }
    let source = wide(source)?;
    let destination = wide(destination)?;
    // No COPY_ALLOWED: publication must be a same-filesystem move.
    // SAFETY: the NUL-terminated buffers remain live throughout the call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            flags | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rename canonical state without replacement, durably publishing the name.
///
/// Callers must serialize directory mutations on Unix (the existence check and
/// rename are separate operations). Both paths must be on the same filesystem.
///
/// # Errors
/// Returns filesystem errors; a failure after rename requires retry/recovery.
pub fn rename_new(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows_move(source, destination, 0)
    }
    #[cfg(not(windows))]
    {
        if destination.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination exists",
            ));
        }
        std::fs::rename(source, destination)?;
        sync_parent(destination)?;
        if source.parent() != destination.parent() {
            sync_parent(source)?;
        }
        Ok(())
    }
}

#[cfg(not(windows))]
fn sync_parent(path: &Path) -> io::Result<()> {
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("missing parent"))?,
    )?
    .sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_is_complete_and_never_replaces_state() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        publish_new(&path, b"original").unwrap();
        assert!(publish_new(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn replacement_publishes_complete_values_and_cleans_failed_stages() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        replace(&path, b"original").unwrap();
        replace(&path, b"replacement").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        let directory = root.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(replace(&directory, b"invalid").is_err());
        assert!(directory.is_dir());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn directory_moves_preserve_state_and_reject_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old");
        let new = root.path().join("new");
        std::fs::create_dir(&old).unwrap();
        publish_new(&old.join("state"), b"original").unwrap();
        rename_new(&old, &new).unwrap();
        assert_eq!(std::fs::read(new.join("state")).unwrap(), b"original");
        std::fs::create_dir(&old).unwrap();
        assert!(rename_new(&old, &new).is_err());
        assert!(old.exists());
    }
}
