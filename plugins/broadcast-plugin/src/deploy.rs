//! Explicit, plugin-owned deployment entry point; no startup/cloud side effects.
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const ASSETS: &[(&str, &[u8])] = &[
    (
        "live-install.py",
        include_bytes!("../../../scripts/live-install.py"),
    ),
    (
        "configure-live-imports.py",
        include_bytes!("../../../scripts/configure-live-imports.py"),
    ),
    (
        "live-state.py",
        include_bytes!("../../../scripts/live-state.py"),
    ),
    (
        "smoke-live-broadcast.py",
        include_bytes!("../../../scripts/smoke-live-broadcast.py"),
    ),
    (
        "main.tf",
        include_bytes!("../../../infra/live-terraform/main.tf"),
    ),
    (
        ".terraform.lock.hcl",
        include_bytes!("../../../infra/live-terraform/.terraform.lock.hcl"),
    ),
    (
        "state.tf",
        include_bytes!("../../../infra/live-terraform/state.tf"),
    ),
    (
        "encryption.tf",
        include_bytes!("../../../infra/live-terraform/encryption.tf"),
    ),
    (
        "archive-live-state.py",
        include_bytes!("../../../scripts/archive-live-state.py"),
    ),
];

fn private_directory(path: &Path) -> Result<(), PluginCommandError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| PluginCommandError::failed("deployment cache directory unavailable"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| PluginCommandError::failed("deployment cache metadata unavailable"))?;
    // SAFETY: geteuid has no preconditions and does not change process state.
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        return Err(PluginCommandError::failed(
            "deployment cache must be owned, non-symlink and private",
        ));
    }
    Ok(())
}

fn installer(state: &Path) -> Result<PathBuf, PluginCommandError> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut identity = Sha256::new();
    for (name, bytes) in ASSETS {
        identity.update(name.as_bytes());
        identity.update((bytes.len() as u64).to_le_bytes());
        identity.update(bytes);
    }
    let parent = state.join("broadcast-deploy-installer");
    private_directory(&parent)?;
    let hash = identity
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root = parent.join(hash);
    private_directory(&root)?;
    for (name, bytes) in ASSETS {
        let path = root.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|_| {
                        PluginCommandError::failed("deployment installer extraction failed")
                    })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => {
                return Err(PluginCommandError::failed(
                    "deployment installer cache unavailable",
                ));
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| PluginCommandError::failed("deployment installer asset unavailable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| PluginCommandError::failed("deployment installer metadata unavailable"))?;
        // SAFETY: geteuid has no preconditions and does not change process state.
        let owner = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.uid() != owner
            || metadata.mode() & 0o077 != 0
            || metadata.len() != bytes.len() as u64
        {
            return Err(PluginCommandError::failed(
                "deployment installer asset ownership/type mismatch",
            ));
        }
        let mut actual = Vec::with_capacity(bytes.len());
        file.read_to_end(&mut actual)
            .map_err(|_| PluginCommandError::failed("deployment installer verification failed"))?;
        if actual.as_slice() != *bytes {
            return Err(PluginCommandError::failed(
                "deployment installer differs from compiled identity; remove corrupt private cache",
            ));
        }
    }
    std::fs::File::open(&root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| PluginCommandError::failed("deployment installer cache durability failed"))?;
    Ok(root.join("live-install.py"))
}

fn installed_candidate(executable: &Path) -> Result<PathBuf, PluginCommandError> {
    if !executable.is_absolute() {
        return Err(PluginCommandError::failed(
            "selected executable path must be absolute",
        ));
    }
    executable
        .parent()
        .map(|parent| parent.join("live-candidate"))
        .ok_or_else(|| {
            PluginCommandError::failed("selected executable has no installation directory")
        })
}

pub(crate) async fn run(
    context: &NativeCommandContext,
    args: &[String],
) -> Result<i32, PluginCommandError> {
    let state = PathBuf::from(&context.connection.state_dir);
    let script = installer(&state)?;
    // Only the interpreter is resolved through PATH; installer code is compile-verified.
    let mut forwarded = Vec::new();
    let mut explicit_python = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--python" {
            index += 1;
            let value = args.get(index).ok_or_else(|| {
                PluginCommandError::failed("--python requires an absolute interpreter path")
            })?;
            let path = PathBuf::from(value);
            if !path.is_absolute() || explicit_python.is_some() {
                return Err(PluginCommandError::failed(
                    "--python requires one absolute interpreter path",
                ));
            }
            explicit_python = Some(path);
        } else {
            forwarded.push(args[index].clone());
        }
        index += 1;
    }
    let python = if let Some(path) = explicit_python {
        path
    } else {
        std::env::var_os("PATH")
            .and_then(|value| {
                std::env::split_paths(&value)
                    .take(128)
                    .filter(|directory| directory.is_absolute())
                    .map(|directory| directory.join("python3"))
                    .find(|path| path.is_file())
            })
            .ok_or_else(|| {
                PluginCommandError::failed("install Python >=3.12 or pass --python ABSOLUTE_PATH")
            })?
    };
    let python = python
        .canonicalize()
        .map_err(|_| PluginCommandError::failed("Python interpreter path unavailable"))?;
    let mut probe = tokio::process::Command::new(&python);
    probe
        .arg("-I")
        .arg("-c")
        .arg("import sys; sys.exit(0 if sys.version_info >= (3,12) else 2)")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let checked = tokio::time::timeout(std::time::Duration::from_secs(5), probe.status())
        .await
        .map_err(|_| PluginCommandError::failed("Python version probe timed out"))?
        .map_err(|_| PluginCommandError::failed("Python version probe unavailable"))?;
    if !checked.success() {
        return Err(PluginCommandError::failed(
            "Python >=3.12 required; select trusted interpreter with --python ABSOLUTE_PATH",
        ));
    }
    let executable = std::env::current_exe()
        .map_err(|_| PluginCommandError::failed("selected executable identity unavailable"))?;
    let default_candidate = installed_candidate(&executable)?;
    let status = tokio::process::Command::new(python)
        .arg("-I")
        .arg(script)
        .arg("--state-dir")
        .arg(state.join("broadcast-deploy"))
        .arg("--default-artifact")
        .arg(default_candidate)
        .args(forwarded)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
        .map_err(|_| {
            PluginCommandError::failed(
                "deployment installer could not start; check Python 3 installation",
            )
        })?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_default_uses_selected_executable_not_cache_or_cwd() {
        assert_eq!(
            installed_candidate(Path::new("/opt/bmux/slots/test/bin/bmux"))
                .expect("absolute install"),
            PathBuf::from("/opt/bmux/slots/test/bin/live-candidate")
        );
        assert!(installed_candidate(Path::new("relative/bmux")).is_err());
    }

    #[test]
    fn embedded_cache_reuses_exact_assets_and_rejects_tampering() {
        let state = tempfile::tempdir().expect("private test state");
        let path = installer(state.path()).expect("extract compiled assets");
        assert_eq!(std::fs::read(&path).expect("read installer"), ASSETS[0].1);
        assert_eq!(installer(state.path()).expect("reuse verified cache"), path);
        std::fs::write(&path, b"untrusted replacement").expect("tamper owned fixture");
        assert!(installer(state.path()).is_err());
    }

    #[test]
    fn embedded_cache_rejects_symlink_asset_and_public_directory() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let state = tempfile::tempdir().expect("private test state");
        let path = installer(state.path()).expect("extract compiled assets");
        let outside = state.path().join("outside.py");
        std::fs::write(&outside, ASSETS[0].1).expect("fixture asset");
        std::fs::remove_file(&path).expect("remove exact fixture asset");
        symlink(outside, &path).expect("symlink fixture");
        assert!(installer(state.path()).is_err());
        std::fs::set_permissions(
            path.parent().expect("cache parent"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("public fixture cache");
        assert!(installer(state.path()).is_err());
    }
}
