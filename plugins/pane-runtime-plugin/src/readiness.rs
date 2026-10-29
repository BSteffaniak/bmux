//! Activation-time readiness and conservative execution admission fencing.
use bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::RuntimeReadiness;
use bmux_plugin_sdk::NativeLifecycleContext;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

static STATE: OnceLock<Mutex<RuntimeReadiness>> = OnceLock::new();
static HELPER: OnceLock<Option<(std::path::PathBuf, String)>> = OnceLock::new();
static EXECUTABLE: OnceLock<ExecutableIdentity> = OnceLock::new();

/// Capture and cache executable identity during activation, before serving IPC.
/// Readiness requests must never trigger a cold executable hash.
struct ExecutableIdentity {
    file: Mutex<Option<std::fs::File>>,
    digest: OnceLock<Result<String, String>>,
}

impl ExecutableIdentity {
    fn capture() -> Self {
        #[cfg(target_os = "linux")]
        let opened = std::fs::File::open("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let opened = running_executable_sha256().ok_or_else(|| {
            std::io::Error::other("immutable bundle startup attestation unavailable")
        });
        #[cfg(not(target_os = "linux"))]
        return Self {
            file: Mutex::new(None),
            digest: OnceLock::from(opened.map_err(|error| error.to_string())),
        };
        #[cfg(target_os = "linux")]
        let digest = OnceLock::new();
        #[cfg(target_os = "linux")]
        let file = match opened {
            Ok(file) => Some(file),
            Err(error) => {
                let _ = digest.set(Err(format!(
                    "activation executable identity unavailable: {error}"
                )));
                None
            }
        };
        #[cfg(target_os = "linux")]
        {
            Self {
                file: Mutex::new(file),
                digest,
            }
        }
    }

    fn get(&self) -> Result<String, String> {
        self.digest
            .get_or_init(|| {
                let file = self
                    .file
                    .lock()
                    .map_err(|_| "executable identity lock poisoned")?
                    .take()
                    .ok_or("activation executable handle unavailable")?;
                sha256_reader(file)
                    .map_err(|error| format!("activation executable identity failed: {error}"))
            })
            .clone()
    }
}

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
    sha256_reader(std::fs::File::open(path)?)
}

fn sha256_reader(mut input: impl std::io::Read) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    // Debug hosts can exceed a gigabyte. Keep memory independent of artifact
    // size while amortizing read/syscall overhead; allocate off the stack so
    // activation does not consume a Tokio worker's stack with a large array.
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = match input.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
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
    let identity = ExecutableIdentity::capture();
    let verified = identity.get();
    let executable_sha256 = verified.as_ref().ok().cloned();
    let managed_target_identity = std::env::var("BMUX_MANAGED_TARGET_IDENTITY").ok();
    // Ordinary non-Linux launches may lack mapped-image attestation. Never
    // substitute a mutable current_exe pathname hash for that proof.
    if (cfg!(target_os = "linux") || managed_target_identity.is_some())
        && let Err(reason) = verified
    {
        error.get_or_insert(reason);
    }
    let _ = EXECUTABLE.set(identity);
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
    // Identity verification completed before activation published STATE.
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
    fn executable_identity_is_lazy_cached_and_failure_is_explicit() {
        let mut file = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut file, b"abc").unwrap();
        std::io::Seek::rewind(&mut file).unwrap();
        let identity = ExecutableIdentity {
            file: Mutex::new(Some(file)),
            digest: OnceLock::new(),
        };
        assert!(identity.digest.get().is_none());
        assert!(identity.file.lock().unwrap().is_some());
        let digest = identity.get().unwrap();
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(identity.file.lock().unwrap().is_none());
        assert_eq!(identity.get().unwrap(), digest);
        let unavailable = ExecutableIdentity {
            file: Mutex::new(None),
            digest: OnceLock::new(),
        };
        assert!(
            unavailable
                .get()
                .unwrap_err()
                .contains("handle unavailable")
        );
        assert!(unavailable.digest.get().unwrap().is_err());
    }
    #[test]
    fn cold_identity_hashes_pinned_artifact_and_survives_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("activation-executable");
        std::fs::write(&path, b"abc").unwrap();
        let identity = ExecutableIdentity {
            file: Mutex::new(Some(std::fs::File::open(&path).unwrap())),
            digest: OnceLock::new(),
        };
        // A later deployment must not change which executable the first, cold
        // readiness call attests. Never reopen the pathname to accelerate it.
        std::fs::rename(&path, root.path().join("retained-executable")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        std::thread::scope(|scope| {
            let first = scope.spawn(|| identity.get().unwrap());
            let second = scope.spawn(|| identity.get().unwrap());
            assert_eq!(first.join().unwrap(), expected);
            assert_eq!(second.join().unwrap(), expected);
        });
        assert!(identity.file.lock().unwrap().is_none());
        std::fs::remove_file(path).unwrap();
        assert_eq!(identity.get().unwrap(), expected);
    }

    #[test]
    fn streamed_sha256_handles_boundaries_interrupts_and_failures() {
        use std::io::Read as _;
        struct InterruptedOnce(bool);
        impl std::io::Read for InterruptedOnce {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.0 = false;
                b"abc".as_slice().read(output)
            }
        }
        struct Failed;
        impl std::io::Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        }
        let bytes = vec![b'x'; 1024 * 1024 + 17];
        let expected =
            Sha256::digest(&bytes)
                .iter()
                .fold(String::with_capacity(64), |mut output, byte| {
                    write!(&mut output, "{byte:02x}").unwrap();
                    output
                });
        assert_eq!(sha256_reader(bytes.as_slice()).unwrap(), expected);
        assert_eq!(
            sha256_reader(&b""[..]).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Limit this repeatable reader to one payload after the initial EINTR.
        assert_eq!(
            sha256_reader(InterruptedOnce(false).take(3)).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_reader(Failed).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
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
