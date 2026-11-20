//! Content-addressed auxiliary images survive replacement of their source package.
//! There is deliberately no automatic pruning: activation lifetime does not prove
//! that an auxiliary process (or its recovery work) has finished.
use super::open_auxiliary_artifact;

#[cfg(all(test, unix))]
#[path = "retained/tests.rs"]
mod tests;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

/// Atomically retain a verified executable under an owner-scoped digest path.
/// Existing images are verified and never overwritten; no automatic pruning occurs.
///
/// # Errors
/// Rejects unsafe directories, invalid digests, invalid executable files, and
/// source bytes that do not match the caller-selected SHA-256 identity.
pub fn retain(source: &Path, root: &Path, owner: &str, expected: &str) -> Result<PathBuf, String> {
    let result = (|| {
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid executable SHA-256 identity".into());
        }
        if !root.is_absolute() {
            return Err("auxiliary image root must be absolute".into());
        }
        // Hash owner names rather than interpreting arbitrary plugin IDs as paths.
        let directory = root.join(hex(&Sha256::digest(owner.as_bytes())));
        private_directory(root)?;
        private_directory(&directory)?;
        let directory = directory
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let filename = source
            .file_name()
            .ok_or("auxiliary image has no filename")?;
        let directory = directory.join(expected);
        private_directory(&directory)?;
        let destination = directory.join(filename);
        match std::fs::symlink_metadata(&destination) {
            Ok(_) => {
                verify(&destination, expected)?;
                return Ok(destination);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let (mut input, _) = open_auxiliary_artifact(source)?;
        let mut staging = stage(&mut input, &directory)?;
        verify_file(staging.as_file_mut(), expected)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            staging
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o500))
                .map_err(|error| error.to_string())?;
        }
        staging
            .as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        // The rename keeps the staged inode verified above; only an image
        // published concurrently by someone else still needs checking.
        let published = match staging.persist_noclobber(&destination) {
            Ok(_) => true,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrent publisher won. Never overwrite even a corrupt image.
                false
            }
            Err(error) => return Err(error.to_string()),
        };
        #[cfg(unix)]
        File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        if !published {
            verify(&destination, expected)?;
        }
        Ok(destination)
    })();
    result.map_err(|error: String| {
        format!(
            "retain auxiliary executable {} in {}: {error}",
            source.display(),
            root.display()
        )
    })
}

/// Stage a private copy of the opened source, never a hard link to mutable
/// build output. On APFS a clone shares the source's blocks instead of
/// rewriting and syncing a whole (possibly very large) executable.
fn stage(input: &mut File, directory: &Path) -> Result<tempfile::NamedTempFile, String> {
    #[cfg(target_os = "macos")]
    if let Ok(staging) = tempfile::Builder::new().make_in(directory, |path| clone(input, path)) {
        return Ok(staging);
    }
    let mut staging =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    std::io::copy(input, staging.as_file_mut()).map_err(|error| error.to_string())?;
    Ok(staging)
}

/// Clone the already-opened source (not its path) to `path`. Clones inherit
/// file flags, so an immutable source must not yield an immutable staging file.
#[cfg(target_os = "macos")]
pub(super) fn clone(input: &File, path: &Path) -> std::io::Result<File> {
    use std::os::unix::{ffi::OsStrExt as _, io::AsRawFd as _};
    let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidInput);
    let parent = File::open(path.parent().ok_or_else(invalid)?)?;
    let name = std::ffi::CString::new(path.file_name().ok_or_else(invalid)?.as_bytes())
        .map_err(|_| invalid())?;
    // SAFETY: both descriptors stay open for the call and `name` is NUL-terminated.
    if unsafe { libc::fclonefileat(input.as_raw_fd(), parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let cloned = File::open(path).and_then(|file| {
        // SAFETY: `file` is an open descriptor owned by this process.
        if unsafe { libc::fchflags(file.as_raw_fd(), 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(file)
    });
    if cloned.is_err() {
        let _ = std::fs::remove_file(path);
    }
    cloned
}

fn private_directory(path: &Path) -> Result<(), String> {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match builder.create(path) {
        Ok(()) => {
            #[cfg(unix)]
            File::open(path.parent().ok_or("image directory has no parent")?)
                .and_then(|file| file.sync_all())
                .map_err(|error| error.to_string())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "image directory is not a real directory: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid only reads the effective user identity.
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "image directory must be private and user-owned: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn verify(path: &Path, expected: &str) -> Result<(), String> {
    let (mut file, _) = open_auxiliary_artifact(path)?;
    verify_file(&mut file, expected).map_err(|error| {
        format!(
            "invalid retained auxiliary image {}: {error}",
            path.display()
        )
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("writing to a string cannot fail");
    }
    value
}

fn verify_file(file: &mut File, expected: &str) -> Result<(), String> {
    file.rewind().map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    if hex(&digest.finalize()) != expected {
        return Err("auxiliary image checksum mismatch".into());
    }
    Ok(())
}
