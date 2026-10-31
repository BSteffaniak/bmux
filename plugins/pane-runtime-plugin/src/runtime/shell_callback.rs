//! Retain the exact shell callback image before build outputs can disappear.
//!
//! These images deliberately outlive the manager: nested shells and recovery may
//! still depend on them. They must not be pruned as part of pane teardown.

use anyhow::{Context as _, Result, ensure};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};

pub(super) fn prepare(root: &Path) -> Result<PathBuf> {
    // Managers prepare eagerly, before the first pane (or cargo clean). Cache by
    // runtime root so later panes do not reopen a deleted/rebuilt source image.
    static IMAGES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<PathBuf, PathBuf>>,
    > = std::sync::OnceLock::new();
    let mut images = IMAGES
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("shell callback image lock poisoned"))?;
    if let Some(path) = images.get(root) {
        return Ok(path.clone());
    }
    let source = std::env::current_exe().context("locate shell callback executable")?;
    let path = retain(root, &source)?;
    images.insert(root.to_owned(), path.clone());
    Ok(path)
}

fn digest(file: &mut std::fs::File) -> Result<String> {
    file.rewind()?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            let mut encoded = String::with_capacity(64);
            for byte in hash.finalize() {
                write!(&mut encoded, "{byte:02x}")?;
            }
            return Ok(encoded);
        }
        hash.update(&buffer[..count]);
    }
}

fn retain(root: &Path, source: &Path) -> Result<PathBuf> {
    let directory = root.join("callback-images");
    std::fs::create_dir_all(&directory).context("create retained shell callback directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    // Copy, never hard-link: rebuilding or truncating the build output must not
    // change the retained image. Publish only complete, executable bytes.
    let mut source = std::fs::File::open(source).context("open shell callback executable")?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    std::io::copy(&mut source, temporary.as_file_mut())?;
    let hash = digest(temporary.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o500))?;
    }
    temporary.as_file().sync_all()?;
    let path = directory.join(format!("{hash}-bmux{}", std::env::consts::EXE_SUFFIX));
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                std::fs::symlink_metadata(&path)?.file_type().is_file(),
                "retained shell callback is not a regular file"
            );
            ensure!(
                digest(&mut std::fs::File::open(&path)?)? == hash,
                "retained shell callback digest mismatch"
            );
        }
        Err(error) => return Err(error.error).context("publish retained shell callback"),
    }
    path.canonicalize()
        .context("resolve retained shell callback")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn callback_runs_after_build_output_is_deleted() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("target/bmux");
        std::fs::create_dir(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "#!/bin/sh\nprintf retained-callback").unwrap();
        let retained = retain(&temporary.path().join("runtime"), &source).unwrap();
        std::fs::remove_file(&source).unwrap();
        std::fs::remove_dir(source.parent().unwrap()).unwrap();
        let output = std::process::Command::new(retained).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"retained-callback");
    }

    #[test]
    fn rebuilding_source_does_not_modify_retained_image() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("bmux");
        let runtime = temporary.path().join("runtime");
        std::fs::write(&source, b"original image").unwrap();
        let original = retain(&runtime, &source).unwrap();
        assert_eq!(retain(&runtime, &source).unwrap(), original);
        std::fs::write(&source, b"rebuilt image").unwrap();
        let rebuilt = retain(&runtime, &source).unwrap();
        assert_ne!(original, rebuilt);
        assert_eq!(std::fs::read(original).unwrap(), b"original image");
        assert_eq!(std::fs::read(rebuilt).unwrap(), b"rebuilt image");
    }

    #[test]
    fn corrupted_retained_image_is_rejected_not_overwritten() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("bmux");
        let runtime = temporary.path().join("runtime");
        std::fs::write(&source, b"original image").unwrap();
        let retained = retain(&runtime, &source).unwrap();
        // Replace rather than truncate the read-only retained image.
        std::fs::remove_file(&retained).unwrap();
        std::fs::write(&retained, b"corrupt image").unwrap();
        assert!(retain(&runtime, &source).is_err());
        assert_eq!(std::fs::read(retained).unwrap(), b"corrupt image");
    }
}
