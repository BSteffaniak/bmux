//! Native development entry point. Bundles are replaceable; runtime state is not.
use crate::artifacts::{Result, error};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// Select a Cargo build profile, never an arbitrary artifact path. Cargo remains
/// responsible for checking that custom profiles exist in the workspace manifest.
pub(crate) fn profile(requested: Option<&str>) -> Result<&str> {
    let profile = requested.unwrap_or("dev");
    if profile.is_empty()
        || profile.starts_with('-')
        || !profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || matches!(
            profile,
            "debug" | "build" | "check" | "doc" | "test" | "bench"
        )
    {
        return Err(error(
            "invalid bterm build profile: use dev, release, profiling, or a named Cargo build profile (not a path or test/bench profile)",
        ));
    }
    Ok(profile)
}

fn profile_directory(profile: &str) -> &str {
    if profile == "dev" { "debug" } else { profile }
}

fn build_arguments(
    target: &str,
    profile: &str,
    declarations: &[serde_json::Value],
) -> Result<Vec<String>> {
    let profile = self::profile(Some(profile))?;
    let mut arguments = crate::build::arguments(
        target,
        profile,
        declarations,
        &crate::build::Features::default(),
    )?;
    arguments.extend(["-p".into(), "bterm".into(), "--bin".into(), "bterm".into()]);
    Ok(arguments)
}

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

/// Existing development installation identity, deliberately stable across builds.
///
/// This is not a runtime compatibility key: Bterm owns artifact/configuration
/// routing and short transport paths beneath its selected installation. Keep the
/// established workspace mapping so rebuilding never silently abandons durable
/// development data or switches to the canonical installed Bterm's roots.
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
    // Do not predict boot IDs, compatibility namespaces, socket suffixes, or
    // sockaddr limits here. The selected Bterm validates its actual transport
    // after resolving artifact/configuration/scope identity.
    Ok(base.join("bterm-dev").join(identity))
}

/// Exact immutable generation selected by the completed build.
pub(crate) struct BuiltBundle {
    executable: PathBuf,
}

/// Build, replace and verify the bundle without consulting or starting a runtime.
pub(crate) fn build(workspace: &Path, profile: &str) -> Result<BuiltBundle> {
    let profile = self::profile(Some(profile))?;
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
    let arguments = build_arguments(
        &target,
        profile,
        &crate::artifacts::declarations(workspace)?,
    )?;
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
        &target_dir.join(&target).join(profile_directory(profile)),
        &bundle,
        &target,
    )?;
    eprintln!("Built {}", bundle.display());
    Ok(BuiltBundle {
        executable: executable_root(&bundle, &target).join("bterm"),
    })
}

pub(crate) fn run(workspace: &Path, profile: &str) -> Result<()> {
    let built = build(workspace, profile)?;
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
    fn build_profiles_default_map_and_reject_unsafe_or_reserved_names() {
        assert_eq!(profile(None).unwrap(), "dev");
        for (selected, directory) in [
            ("dev", "debug"),
            ("release", "release"),
            ("profiling", "profiling"),
            ("custom-fast_2", "custom-fast_2"),
        ] {
            assert_eq!(profile(Some(selected)).unwrap(), selected);
            assert_eq!(profile_directory(selected), directory);
        }
        for selected in [
            "",
            "debug",
            "test",
            "bench",
            "build",
            "check",
            "doc",
            "../release",
            "/tmp/release",
            "a/b",
            "a\\b",
            "--release",
            "release ",
            ".",
            "..",
        ] {
            assert!(profile(Some(selected)).is_err(), "accepted {selected:?}");
        }
    }

    #[test]
    fn every_product_artifact_uses_the_exact_selected_profile() {
        let declarations = vec![serde_json::json!({
            "cargo_package": "worker_plugin", "cargo_binary": "worker"
        })];
        for profile in ["dev", "release", "profiling", "custom-fast_2"] {
            let arguments =
                build_arguments("aarch64-apple-darwin", profile, &declarations).unwrap();
            assert_eq!(
                arguments.iter().filter(|arg| *arg == "--profile").count(),
                1
            );
            assert!(
                arguments
                    .windows(2)
                    .any(|pair| pair == ["--profile", profile])
            );
            for binary in ["bmux", "xtask", "worker", "bterm"] {
                assert!(arguments.windows(2).any(|pair| pair == ["--bin", binary]));
            }
            assert!(!arguments.iter().any(|arg| arg == "--release"));
        }
    }

    #[test]
    fn cli_rejects_invalid_profiles_before_build_or_launch() {
        for operation in ["build", "run"] {
            for profile in ["../release", "debug", "test", "bench"] {
                let error = crate::run(vec![
                    "bterm".into(),
                    operation.into(),
                    "--profile".into(),
                    profile.into(),
                ])
                .err()
                .unwrap();
                assert!(error.to_string().contains("invalid bterm build profile"));
            }
        }
        for operation in ["stage", "verify"] {
            let error = crate::run(vec![
                "bterm".into(),
                operation.into(),
                "--profile".into(),
                "release".into(),
            ])
            .err()
            .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("only supported for bterm build/run")
            );
        }
    }

    #[test]
    fn repeated_refresh_replaces_bundle_and_preserves_state_and_failed_build() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
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
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let bundle = temp.path().join("bundle");
        std::os::unix::fs::symlink(&outside, &bundle).unwrap();
        assert!(refresh(temp.path(), temp.path(), &bundle, "aarch64-apple-darwin").is_err());
        assert!(outside.is_dir());
    }
}
