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

pub(super) fn retain(
    source: &Path,
    root: &Path,
    owner: &str,
    expected: &str,
) -> Result<PathBuf, String> {
    let result = (|| {
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
        let mut staging =
            tempfile::NamedTempFile::new_in(&directory).map_err(|error| error.to_string())?;
        // Always copy from an opened source, never hard-link mutable build output.
        std::io::copy(&mut input, staging.as_file_mut()).map_err(|error| error.to_string())?;
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
        match staging.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrent publisher won. Never overwrite even a corrupt image.
            }
            Err(error) => return Err(error.to_string()),
        }
        #[cfg(unix)]
        File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        verify(&destination, expected)?;
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

fn private_directory(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
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
