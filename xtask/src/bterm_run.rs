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

fn sync_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(error("bundle publication rejects symlinks"));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_tree(&entry?.path())?;
        }
        #[cfg(unix)]
        fs::File::open(path)?.sync_all()?;
    } else if metadata.is_file() {
        fs::File::open(path)?.sync_all()?;
    } else {
        return Err(error("bundle publication rejects special files"));
    }
    Ok(())
}

/// Stage and verify before publishing a content-addressed immutable bundle.
/// Existing generations stay in place for live daemons and their helper paths.
fn refresh(workspace: &Path, source: &Path, bundle: &Path, target: &str) -> Result<PathBuf> {
    use sha2::{Digest as _, Sha256};
    let parent = bundle
        .parent()
        .ok_or_else(|| error("bundle needs a parent"))?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(bundle)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(error("refusing non-directory legacy bundle reference"));
    }
    let next = staging.path().join("next");
    crate::bterm::stage(workspace, source, &next, target)?;
    let root = executable_root(&next, target);
    crate::bterm::verify(&root, target)?;
    let mut hasher = Sha256::new();
    hasher.update(fs::read(root.join("bterm-bundle.json"))?);
    for relative in ["Contents/Info.plist", "share/applications/bterm.desktop"] {
        let path = next.join(relative);
        match fs::read(path) {
            Ok(bytes) => {
                hasher.update(relative.as_bytes());
                hasher.update(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let identity = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let generations = parent.join("bundles");
    crate::artifacts::safe_path(&generations)?;
    fs::create_dir_all(&generations)?;
    let generation = generations.join(&identity);
    crate::artifacts::safe_path(&generation)?;
    fs::create_dir_all(&generation)?;
    let published = generation.join(
        bundle
            .file_name()
            .ok_or_else(|| error("bundle filename missing"))?,
    );
    crate::artifacts::safe_path(&published)?;
    if published.try_exists()? {
        crate::bterm::verify(&executable_root(&published, target), target)?;
        if fs::read(executable_root(&published, target).join("bterm-bundle.json"))?
            != fs::read(root.join("bterm-bundle.json"))?
        {
            return Err(error("immutable bundle identity collision"));
        }
        for relative in ["Contents/Info.plist", "share/applications/bterm.desktop"] {
            let expected = next.join(relative);
            let actual = published.join(relative);
            if expected.try_exists()? {
                if fs::symlink_metadata(&actual)?.file_type().is_symlink()
                    || fs::read(&expected)? != fs::read(&actual)?
                {
                    return Err(error("immutable bundle launcher metadata is corrupt"));
                }
            } else if actual.try_exists()? {
                return Err(error("unexpected immutable bundle launcher metadata"));
            }
        }
    } else {
        sync_tree(&next)?;
        fs::rename(next, &published)?;
        #[cfg(unix)]
        fs::File::open(&generation)?.sync_all()?;
    }
    // This small reference is informational; execution uses the exact generation path.
    let reference = staging.path().join("latest.json");
    fs::write(
        &reference,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1, "bundle_sha256": identity, "bundle": published,
        }))?,
    )?;
    fs::File::open(&reference)?.sync_all()?;
    fs::rename(reference, parent.join("latest.json"))?;
    #[cfg(unix)]
    {
        fs::File::open(&generations)?.sync_all()?;
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(published)
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

/// Exact immutable generation selected by the completed build.
pub(crate) struct BuiltBundle {
    executable: PathBuf,
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
    // Serialize publication only; running windows use immutable generation paths.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(owned.join("run.lock"))?;
    lock.try_lock()
        .map_err(|err| error(format!("another bterm build/run may be active: {err}")))?;
    let mut arguments =
        crate::build::arguments(&target, "dev", &crate::artifacts::declarations(workspace)?)?;
    arguments.extend([
        "-p".into(),
        "bmux_bterm".into(),
        "--bin".into(),
        "bterm".into(),
    ]);
    let status = cargo().args(arguments).current_dir(workspace).status()?;
    if !status.success() {
        return Err(error(format!("bterm build failed: {status}")));
    }
    let bundle = owned.join(if target.contains("apple-darwin") {
        "Bterm.app"
    } else {
        "Bterm"
    });
    let bundle = refresh(
        workspace,
        &target_dir.join(&target).join("debug"),
        &bundle,
        &target,
    )?;
    eprintln!("Built {}", bundle.display());
    Ok(BuiltBundle {
        executable: executable_root(&bundle, &target).join("bterm"),
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
        for name in ["bterm", "bmux", "xtask", "bmux-pane-execution-supervisor"] {
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
            fs::write(source.join("bterm"), "version one").unwrap();
            let first = refresh(&workspace, &source, &bundle, target).unwrap();
            let again = refresh(&workspace, &source, &bundle, target).unwrap();
            assert_eq!(first, again);
            let first_root = executable_root(&first, target);
            fs::write(source.join("bterm"), "version two").unwrap();
            let second = refresh(&workspace, &source, &bundle, target).unwrap();
            assert_ne!(first, second);
            let root = executable_root(&second, target);
            assert_eq!(
                fs::read_to_string(first_root.join("bterm")).unwrap(),
                "version one"
            );
            assert_eq!(
                fs::read_to_string(root.join("bterm")).unwrap(),
                "version two"
            );
            crate::bterm::verify(&first_root, target).unwrap();
            crate::bterm::verify(&root, target).unwrap();
            let reference = fs::read(owned.join("latest.json")).unwrap();
            assert!(refresh(&workspace, &source.join("absent"), &bundle, target).is_err());
            assert_eq!(fs::read(owned.join("latest.json")).unwrap(), reference);
            crate::bterm::verify(&root, target).unwrap();
            fs::write(root.join("bterm"), "tampered immutable generation").unwrap();
            assert!(refresh(&workspace, &source, &bundle, target).is_err());
            assert_eq!(fs::read(owned.join("latest.json")).unwrap(), reference);
            assert_eq!(
                fs::read_to_string(root.join("bterm")).unwrap(),
                "tampered immutable generation"
            );
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
