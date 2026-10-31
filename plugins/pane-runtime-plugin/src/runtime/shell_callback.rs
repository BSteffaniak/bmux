//! Shell-owned selection and lifetime policy; publication uses the host's generic
//! executable-retention primitive, shared with declared auxiliary executables.
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

pub(super) fn prepare(root: &Path) -> Result<PathBuf> {
    // Prepare before the first pane. Later panes must not reopen build outputs
    // which may have been removed or replaced since this process started.
    static IMAGES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<PathBuf, (PathBuf, String)>>,
    > = std::sync::OnceLock::new();
    let mut images = IMAGES
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("shell callback image lock poisoned"))?;
    std::fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let directory = root.join("callback-images");
    if let Some((path, hash)) = images.get(&root) {
        // Reuse the expected identity, not a fresh digest of possibly corrupt
        // cached bytes. This also works after the original executable is gone.
        return bmux_plugin::retain_executable(path, &directory, "shell-callback", hash)
            .map_err(anyhow::Error::msg);
    }
    let source = std::env::current_exe()
        .context("locate shell callback executable")?
        .canonicalize()?;
    let hash = bmux_plugin::executable_sha256(&source).map_err(anyhow::Error::msg)?;
    let path = bmux_plugin::retain_executable(&source, &directory, "shell-callback", &hash)
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
