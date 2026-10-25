//! Activation-time readiness and conservative execution admission fencing.
use bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::RuntimeReadiness;
use bmux_plugin_sdk::NativeLifecycleContext;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

static STATE: OnceLock<Mutex<RuntimeReadiness>> = OnceLock::new();
static HELPER: OnceLock<Option<(std::path::PathBuf, String)>> = OnceLock::new();

fn helper_error(context: &NativeLifecycleContext) -> Option<String> {
    resolution_error(
        context
            .auxiliary_executables
            .get("execution-supervisor")
            .map(|artifact| artifact.protocol_version),
        context
            .auxiliary_executable_errors
            .get("execution-supervisor")
            .map(String::as_str),
    )
}

fn resolution_error(version: Option<u32>, error: Option<&str>) -> Option<String> {
    match version {
        Some(version) if version == crate::supervisor_protocol::VERSION => None,
        Some(version) => Some(format!(
            "execution supervisor protocol mismatch: expected {}, got {version}",
            crate::supervisor_protocol::VERSION
        )),
        None => Some(
            error
                .unwrap_or("execution supervisor was not resolved at plugin activation")
                .to_owned(),
        ),
    }
    .map(|cause| {
        format!(
            "{cause}; install/rebuild the matching pane-runtime plugin package with its \
             bmux-pane-execution-supervisor auxiliary artifact in the selected runtime's \
             authoritative package location, then explicitly restart that runtime when safe; \
             PATH helpers and direct-PTY fallback are not supported"
        )
    })
}

fn sha256(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        }))
}

/// Linux exposes the mapped executable even after its directory entry is replaced.
/// Do not canonicalize this procfs handle into the mutable installation pathname.
#[cfg(target_os = "linux")]
fn running_executable_sha256() -> Option<String> {
    sha256(std::path::Path::new("/proc/self/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
fn running_executable_sha256() -> Option<String> {
    // This is an explicit startup attestation, not a hash of current_exe(). The
    // application must verify its immutable bundle/manifest before activation.
    std::env::var("BMUX_MANAGED_EXECUTABLE_SHA256")
        .ok()
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

pub(crate) fn activate(context: &NativeLifecycleContext) {
    let mut error = helper_error(context);
    let _ = HELPER.set(
        context
            .auxiliary_executables
            .get("execution-supervisor")
            .map(|artifact| {
                (
                    std::path::PathBuf::from(&artifact.path),
                    artifact.artifact_sha256.clone(),
                )
            }),
    );
    let executable_sha256 = running_executable_sha256();
    let managed_target_identity = std::env::var("BMUX_MANAGED_TARGET_IDENTITY").ok();
    if managed_target_identity.is_some() && executable_sha256.is_none() {
        error.get_or_insert_with(|| "running executable identity unavailable; immutable bundle startup attestation required on this platform".into());
    }
    let _ = STATE.set(Mutex::new(RuntimeReadiness {
        instance: uuid::Uuid::new_v4(),
        process_id: std::process::id(),
        executable_sha256,
        managed_target_identity,
        runtime_dir: context.connection.runtime_dir.clone(),
        config_dir: context.connection.config_dir.clone(),
        data_dir: context.connection.data_dir.clone(),
        state_dir: context.connection.state_dir.clone(),
        helper_ready: error.is_none(),
        helper_error: error,
        restart_blocker: None,
        fenced: false,
    }));
}

fn blocker(state: &RuntimeReadiness) -> Option<String> {
    // Only an activation that could never spawn may be recovered automatically.
    if state.helper_ready {
        return Some("healthy activation cannot be fenced for automatic restart".into());
    }
    let Some(handle) = crate::handlers::session_runtime_handle() else {
        return Some("execution manager unavailable; restart safety unknown".into());
    };
    if !handle.0.list_session_ids().is_empty() {
        return Some("existing session runtimes prevent automatic restart".into());
    }
    for directory in [
        std::path::PathBuf::from(&state.state_dir).join("pane-executions-v4"),
        std::path::PathBuf::from(&state.runtime_dir),
    ] {
        match std::fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Some(format!(
                    "execution authority directory is not a regular directory: {}",
                    directory.display()
                ));
            }
            Err(error) => return Some(format!("cannot inspect execution authority: {error}")),
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                return Some(format!(
                    "cannot prove restart safety at {}: {error}",
                    directory.display()
                ));
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Some(error.to_string()),
            };
            if directory.ends_with("pane-executions-v4")
                || entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("supervisor-")
            {
                return Some("retained execution authority prevents automatic restart".into());
            }
        }
    }
    None
}

pub(crate) fn snapshot() -> Result<RuntimeReadiness, String> {
    let mut state = STATE
        .get()
        .ok_or("readiness not activated")?
        .lock()
        .map_err(|_| "readiness lock poisoned")?
        .clone();
    state.fenced |= crate::runtime::managed_idle::is_fenced()?;
    state.restart_blocker = blocker(&state);
    if state.helper_ready
        && let Err(error) =
            crate::runtime::reservations::check_authority().and_then(|()| verify_helper())
    {
        state.helper_ready = false;
        state.helper_error = Some(error.to_string());
    }
    Ok(state)
}

pub(crate) fn fence(instance: uuid::Uuid) -> Result<RuntimeReadiness, String> {
    // Never take the execution-manager lock while holding the admission lock:
    // ordinary spawn holds the manager lock before checking admission.
    let observed = snapshot()?;
    let mut state = STATE
        .get()
        .ok_or("readiness not activated")?
        .lock()
        .map_err(|_| "readiness lock poisoned")?;
    apply_fence(&mut state, instance, observed.restart_blocker)?;
    Ok(state.clone())
}

fn apply_fence(
    state: &mut RuntimeReadiness,
    instance: uuid::Uuid,
    blocker: Option<String>,
) -> Result<(), String> {
    if state.instance != instance {
        return Err("runtime instance changed".into());
    }
    if state.helper_ready {
        return Err("healthy activation cannot be fenced for automatic restart".into());
    }
    if let Some(reason) = blocker {
        return Err(reason);
    }
    state.fenced = true;
    Ok(())
}

fn verify_helper() -> anyhow::Result<()> {
    let (path, expected) = HELPER
        .get()
        .and_then(Option::as_ref)
        .ok_or_else(|| anyhow::anyhow!("activation helper identity is unavailable"))?;
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "execution supervisor is no longer a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "execution supervisor is no longer executable"
        );
    }
    let hash = sha256(path)?;
    anyhow::ensure!(
        &hash == expected,
        "execution supervisor checksum no longer matches activation identity"
    );
    Ok(())
}

pub(crate) fn check_admission() -> anyhow::Result<()> {
    if let Some(state) = STATE.get() {
        let state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("readiness lock poisoned"))?;
        anyhow::ensure!(!state.fenced, "runtime fenced for graceful recovery");
        if let Some(reason) = &state.helper_error {
            anyhow::bail!("execution supervisor unavailable: {reason}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_fence_is_repeatable_and_never_fences_healthy_or_unknown_authority() {
        let mut state = RuntimeReadiness {
            instance: uuid::Uuid::new_v4(),
            process_id: 42,
            executable_sha256: Some("a".repeat(64)),
            managed_target_identity: None,
            runtime_dir: "/runtime".into(),
            config_dir: "/config".into(),
            data_dir: "/data".into(),
            state_dir: "/state".into(),
            helper_ready: false,
            helper_error: Some("hash mismatch".into()),
            restart_blocker: None,
            fenced: false,
        };
        let instance = state.instance;
        assert!(apply_fence(&mut state, uuid::Uuid::new_v4(), None).is_err());
        assert!(!state.fenced);
        assert!(apply_fence(&mut state, instance, Some("retained execution".into())).is_err());
        assert!(!state.fenced);
        state.helper_ready = true;
        assert!(apply_fence(&mut state, instance, None).is_err());
        state.helper_ready = false;
        apply_fence(&mut state, instance, None).unwrap();
        apply_fence(&mut state, instance, None).unwrap();
        assert!(state.fenced);
    }
    #[test]
    fn resolution_errors_retain_actual_cause_and_protocol_identity() {
        for error in [
            "missing artifact",
            "checksum mismatch",
            "permission denied",
            "inventory mismatch",
        ] {
            let diagnostic = resolution_error(None, Some(error)).unwrap();
            assert!(diagnostic.starts_with(error));
            assert!(diagnostic.contains("bmux-pane-execution-supervisor"));
            assert!(diagnostic.contains("selected runtime"));
            assert!(diagnostic.contains("explicitly restart that runtime when safe"));
            assert!(diagnostic.contains("PATH helpers and direct-PTY fallback are not supported"));
        }
        let missing = resolution_error(None, None).unwrap();
        assert!(missing.contains("not resolved at plugin activation"));
        assert!(missing.contains("matching pane-runtime plugin package"));
        assert!(
            resolution_error(Some(0), None)
                .unwrap()
                .contains("protocol mismatch")
        );
        assert_eq!(
            resolution_error(Some(crate::supervisor_protocol::VERSION), None),
            None
        );
    }
}
