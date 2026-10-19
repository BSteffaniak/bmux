//! Native development entry point. Bundles are replaceable; runtime state is not.
use crate::artifacts::{Result, error};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

fn executable_root(bundle: &Path, target: &str) -> PathBuf {
    if target.contains("apple-darwin") {
        bundle.join("Contents/MacOS")
    } else {
        bundle.to_owned()
    }
}

/// Build and verify before moving the previous bundle. Roll back publication failure.
/// Only the dedicated development bundle is replaced, never persistent runtime state.
fn refresh(workspace: &Path, source: &Path, bundle: &Path, target: &str) -> Result<()> {
    let parent = bundle
        .parent()
        .ok_or_else(|| error("bundle needs a parent"))?;
    fs::create_dir_all(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(bundle)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(error(
            "refusing to replace a non-directory development bundle",
        ));
    }
    let staging = tempfile::tempdir_in(parent)?;
    let next = staging.path().join("next");
    crate::bterm::stage(workspace, source, &next, target)?;
    crate::bterm::verify(&executable_root(&next, target), target)?;
    let previous = staging.path().join("previous");
    let replacing = bundle.try_exists()?;
    if replacing {
        fs::rename(bundle, &previous)?;
    }
    if let Err(publication) = fs::rename(&next, bundle) {
        if replacing && let Err(rollback) = fs::rename(&previous, bundle) {
            // Retain the backup if rollback fails, instead of deleting the last good bundle.
            let retained = staging.keep();
            return Err(error(format!(
                "bundle publication failed: {publication}; rollback failed: {rollback}; backup retained at {}",
                retained.display()
            )));
        }
        return Err(publication.into());
    }
    Ok(())
}

/// Keep Unix socket paths independent of potentially very long Cargo build roots.
fn managed_root(workspace: &Path) -> Result<PathBuf> {
    use sha2::{Digest as _, Sha256};
    let workspace = workspace.canonicalize()?;
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    let identity = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| error("bterm run requires XDG_STATE_HOME or HOME"))?;
    if !base.is_absolute() {
        return Err(error("bterm development state base must be absolute"));
    }
    let root = base.join("bterm-dev").join(identity);
    // macOS sockaddr_un.sun_path is 104 bytes, including its terminating NUL.
    if root
        .join("runtime/server.sock")
        .as_os_str()
        .as_encoded_bytes()
        .len()
        >= 104
    {
        return Err(error(
            "development socket path is too long; set XDG_STATE_HOME to a shorter absolute path",
        ));
    }
    Ok(root)
}

/// Holds the publication lock until the build-only caller returns or the window exits.
pub(crate) struct BuiltBundle {
    executable: PathBuf,
    _lock: fs::File,
}

/// Build, replace and verify the bundle without consulting or starting a runtime.
pub(crate) fn build(workspace: &Path) -> Result<BuiltBundle> {
    let target = crate::build::host_target(workspace)?;
    if !target.contains("apple-darwin") && !target.contains("linux") {
        return Err(error(
            "bterm build/run currently supports Linux and macOS native backends",
        ));
    }
    // Cargo resolves target-dir configuration and environment overrides itself.
    let metadata = cargo()
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .current_dir(workspace)
        .output()?;
    if !metadata.status.success() {
        return Err(error(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&metadata.stderr)
        )));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let target_dir = PathBuf::from(crate::artifacts::field(&metadata, "target_directory")?);
    let owned = target_dir.join("bterm-run").join(&target);
    fs::create_dir_all(&owned)?;
    // Keep the old bundle stable for a running window and serialize refreshes.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(owned.join("run.lock"))?;
    lock.try_lock()
        .map_err(|err| error(format!("another bterm build/run may be active: {err}")))?;
    let status = cargo()
        .args([
            "build",
            "--locked",
            "--target",
            &target,
            "-p",
            "bmux_bterm",
            "--bin",
            "bterm",
            "-p",
            "bmux_cli",
            "--bin",
            "bmux",
        ])
        .current_dir(workspace)
        .status()?;
    if !status.success() {
        return Err(error(format!("bterm build failed: {status}")));
    }
    let bundle = owned.join(if target.contains("apple-darwin") {
        "Bterm.app"
    } else {
        "Bterm"
    });
    refresh(
        workspace,
        &target_dir.join(&target).join("debug"),
        &bundle,
        &target,
    )?;
    eprintln!("Built {}", bundle.display());
    Ok(BuiltBundle {
        executable: executable_root(&bundle, &target).join("bterm"),
        _lock: lock,
    })
}

pub(crate) fn run(workspace: &Path) -> Result<()> {
    let built = build(workspace)?;
    let state = managed_root(workspace)?;
    eprintln!(
        "Launching {}\nManaged runtime root: {}",
        built.executable.display(),
        state.display()
    );
    let status = Command::new(&built.executable)
        .arg("--managed-root")
        .arg(state)
        .current_dir(workspace)
        .status()?;
    if !status.success() {
        return Err(error(format!("bterm exited unsuccessfully: {status}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_refresh_replaces_bundle_and_preserves_state_and_failed_build() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        for name in ["bterm", "bmux"] {
            fs::write(source.join(name), "version one").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(source.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        for target in ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"] {
            let owned = temp.path().join(target);
            fs::create_dir_all(owned.join("state")).unwrap();
            fs::write(owned.join("state/sentinel"), "persistent").unwrap();
            let bundle = owned.join("Bterm.app");
            refresh(&workspace, &source, &bundle, target).unwrap();
            let root = executable_root(&bundle, target);
            fs::write(root.join("obsolete"), "stale").unwrap();
            fs::write(source.join("bterm"), "version two").unwrap();
            refresh(&workspace, &source, &bundle, target).unwrap();
            assert_eq!(
                fs::read_to_string(root.join("bterm")).unwrap(),
                "version two"
            );
            assert!(!root.join("obsolete").exists());
            crate::bterm::verify(&root, target).unwrap();
            assert!(refresh(&workspace, &source.join("absent"), &bundle, target).is_err());
            crate::bterm::verify(&root, target).unwrap();
            assert_eq!(
                fs::read_to_string(owned.join("state/sentinel")).unwrap(),
                "persistent"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_destination() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let bundle = temp.path().join("bundle");
        std::os::unix::fs::symlink(&outside, &bundle).unwrap();
        assert!(refresh(temp.path(), temp.path(), &bundle, "aarch64-apple-darwin").is_err());
        assert!(outside.is_dir());
    }
}
