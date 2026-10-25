//! Secure the short per-user transport namespace before opening any target file.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};

pub(super) fn ensure_transport_directory(runtime: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid is a read-only identity query.
        let uid = unsafe { libc::geteuid() };
        let parent = std::path::PathBuf::from(format!("/tmp/bt-{uid}"));
        ensure!(
            runtime.parent() == Some(parent.as_path()),
            "managed transport is outside the selected user namespace"
        );
        let name = runtime
            .file_name()
            .context("managed transport has no target name")?
            .to_str()
            .context("managed transport target is not UTF-8")?;
        ensure!(
            name.len() == 64
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid managed transport target name"
        );
        create_private_directory(&parent, uid)?;
        create_private_directory(runtime, uid)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(runtime).context("create managed transport directory")
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("create private managed transport directory"),
    }
    let metadata =
        std::fs::symlink_metadata(path).context("inspect managed transport ownership")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "managed transport directory is not a real directory"
    );
    ensure!(
        metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "managed transport directory is not private to the selected user"
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn directory_validation_preserves_foreign_and_symlink_paths() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = super::super::tests::Root::new();
        // SAFETY: geteuid is a read-only identity query.
        let uid = unsafe { libc::geteuid() };
        let target = root.0.join("target");
        create_private_directory(&target, uid).unwrap();
        create_private_directory(&target, uid).unwrap();
        let link = root.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(create_private_directory(&link, uid).is_err());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(create_private_directory(&target, uid).is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(
            create_private_directory(&root.0.join("wrong-owner"), uid.wrapping_add(1)).is_err()
        );
    }
}
