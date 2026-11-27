//! This process's files and executable, simulated: every file lives in the run's `switchy_fs`
//! file system, which models POSIX (permissions, symbolic links, durability), so the native Unix
//! behaviour applies whatever the kernel's personality. This process's executable is a stand-in
//! file in that file system.
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use switchy_fs::sync::{File, OpenOptions, Permissions};

/// Simulated files track no identity, so every retained image is verified again, exactly as
/// before identities were kept.
pub(crate) const fn file_identity(_path: &Path) -> Option<super::FileIdentity> {
    None
}

/// Where the stand-in for this process's executable lives in the run's file system.
const EXECUTABLE: &str = "/opt/bmux/bin/bmux";

/// This process's id, a stand-in: the simulated kernel runs no process for it, and the host's
/// id would make a run depend on which process ran it.
pub(crate) const fn process_id() -> u32 {
    1
}

/// The stand-in executable's contents.
const EXECUTABLE_IMAGE: &[u8] = b"#!simulated bmux\n";

/// A simulated executable is identified by a digest its launcher attests (the run's
/// environment), as on macOS and Windows: there is no mapped image to pin.
pub(crate) const EXECUTABLE_MAPPED: bool = false;

/// There is no mapped executable image to pin.
pub(crate) fn open_mapped_executable() -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no mapped executable image",
    ))
}

/// The stand-in for this process's executable, created in the run's file system on first use.
///
/// # Errors
/// When the stand-in cannot be created.
pub(crate) fn current_executable() -> std::io::Result<PathBuf> {
    let path = PathBuf::from(EXECUTABLE);
    if !switchy_fs::exists(&path) {
        if let Some(parent) = path.parent() {
            switchy_fs::sync::create_dir_all(parent)?;
        }
        switchy_fs::sync::write(&path, EXECUTABLE_IMAGE)?;
        switchy_fs::sync::set_permissions(&path, Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

/// The SHA-256 digest of the file at `path` in the run's file system.
///
/// # Errors
/// When it cannot be read or is not a file.
fn executable_sha256(path: &Path) -> Result<String, String> {
    let metadata = switchy_fs::sync::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("executable is not a regular file".into());
    }
    let bytes = switchy_fs::sync::read(path).map_err(|error| error.to_string())?;
    Ok(Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        }))
}

/// Retain a copy of `source` (whose digest must be `hash`) under `directory` for `owner`, in
/// the run's file system, as the host's retention does: the copy is named by its digest,
/// verified, and reused once made.
///
/// # Errors
/// When the copy cannot be made or does not match `hash`.
pub(crate) fn retain_executable(
    source: &Path,
    directory: &Path,
    owner: &str,
    hash: &str,
) -> Result<PathBuf, String> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid executable SHA-256 identity".into());
    }
    if !directory.is_absolute() {
        return Err("executable image root must be absolute".into());
    }
    let retained = directory.join(format!("{owner}-{hash}"));
    if executable_sha256(&retained).is_ok_and(|digest| digest == hash) {
        return Ok(retained);
    }
    if executable_sha256(source)? != hash {
        return Err("executable changed before it could be retained".into());
    }
    let bytes = switchy_fs::sync::read(source).map_err(|error| error.to_string())?;
    switchy_fs::sync::create_dir_all(directory).map_err(|error| error.to_string())?;
    restrict_directory(directory).map_err(|error| error.to_string())?;
    let staging = directory.join(format!(".{owner}-{hash}.tmp"));
    let _ = switchy_fs::sync::remove_file(&staging);
    switchy_fs::sync::publish_file(&staging, &retained, &bytes)
        .map_err(|error| error.to_string())?;
    switchy_fs::sync::set_permissions(&retained, Permissions::from_mode(0o500))
        .map_err(|error| error.to_string())?;
    Ok(retained)
}

/// Retain a copy of `source` as it is now under `directory` for `owner`, in the run's file
/// system, and return the copy's path and digest, as the host's single-read retention does.
///
/// # Errors
/// When the copy cannot be made.
pub(crate) fn retain_executable_snapshot(
    source: &Path,
    directory: &Path,
    owner: &str,
) -> Result<(PathBuf, String), String> {
    let hash = executable_sha256(source)?;
    retain_executable(source, directory, owner, &hash).map(|path| (path, hash))
}

/// Make `directory` private to this user.
///
/// # Errors
/// When its permissions cannot be set.
pub(crate) fn restrict_directory(directory: &Path) -> std::io::Result<()> {
    switchy_fs::sync::set_permissions(directory, Permissions::from_mode(0o700))
}

/// Files `options` creates are private to this user.
pub(crate) const fn private_file(options: OpenOptions) -> OpenOptions {
    options.mode(0o600)
}

/// As [`private_file`], and `options` refuses to open through a symbolic link.
pub(crate) const fn private_file_no_follow(options: OpenOptions) -> OpenOptions {
    options.mode(0o600).nofollow(true)
}

/// Durable whole-file replacement.
pub(crate) mod atomic {
    use std::path::Path;
    use switchy_fs::sync::File;

    /// Exclusively create the staging file `path`, private to this user.
    pub(crate) fn create(path: &Path) -> std::io::Result<File> {
        switchy_fs::sync::create_private_file(path)
    }

    /// Replace `path` with `temporary`, and make the replacement durable in `parent`.
    pub(crate) fn publish(temporary: &Path, path: &Path, parent: &Path) -> std::io::Result<()> {
        switchy_fs::sync::rename(temporary, path)?;
        switchy_fs::sync::sync_directory(parent)
    }
}
