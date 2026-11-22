//! Shell-owned selection and lifetime policy; publication uses the host's generic
//! executable-retention primitive.
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

pub(super) fn prepare(root: &Path) -> Result<PathBuf> {
    // Prepare before the first pane. Later panes must not reopen build outputs
    // which may have been removed or replaced since this process started.
    // Process-wide by design (one per simulated run): it pins this process's own executable
    // images.
    enum Images {}
    let images = crate::run_shared::run_shared::<Images, _>(|| {
        switchy_thread::sync::Mutex::new(
            std::collections::BTreeMap::<PathBuf, (PathBuf, String)>::new(),
        )
    });
    let mut images = images
        .lock()
        .map_err(|_| anyhow::anyhow!("shell callback image lock poisoned"))?;
    switchy_fs::sync::create_dir_all(root)?;
    let root = switchy_fs::sync::canonicalize(root)?;
    let directory = root.join("callback-images");
    if let Some((path, hash)) = images.get(&root) {
        // Reuse the expected identity, not a fresh digest of possibly corrupt
        // cached bytes. This also works after the original executable is gone.
        return crate::platform::files::retain_executable(path, &directory, "shell-callback", hash)
            .map_err(anyhow::Error::msg);
    }
    let source = switchy_fs::sync::canonicalize(
        crate::platform::files::current_executable().context("locate shell callback executable")?,
    )?;
    // A managed launch already attested this image's digest before activation;
    // retention still verifies the copied bytes against it, so reuse it rather
    // than reading the whole executable an extra time on the startup path.
    let attested = if crate::platform::files::EXECUTABLE_MAPPED {
        None
    } else {
        crate::readiness::running_executable_sha256()
    };
    let hash = match attested {
        Some(hash) => hash,
        None => crate::platform::files::executable_sha256(&source).map_err(anyhow::Error::msg)?,
    };
    let path =
        crate::platform::files::retain_executable(&source, &directory, "shell-callback", &hash)
            .map_err(anyhow::Error::msg)?;
    // Images outlive this manager: nested shells and recovery can still need
    // them. Pruning is deliberately not part of pane teardown.
    images.insert(root, (path.clone(), hash));
    Ok(path)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn cached_callback_survives_source_removal_and_rejects_corruption() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let source = root.join("bmux");
        std::fs::write(&source, "#!/bin/sh\nprintf retained-callback").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.join("callback-images");
        let hash = bmux_plugin::executable_sha256(&source).unwrap();
        let retained =
            bmux_plugin::retain_executable(&source, &directory, "shell-callback", &hash).unwrap();
        std::fs::remove_file(source).unwrap();
        assert_eq!(
            bmux_plugin::retain_executable(&retained, &directory, "shell-callback", &hash).unwrap(),
            retained
        );
        let output = std::process::Command::new(&retained).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"retained-callback");
        std::fs::set_permissions(&retained, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&retained, "corrupt").unwrap();
        assert!(
            bmux_plugin::retain_executable(&retained, &directory, "shell-callback", &hash).is_err()
        );
    }
}
