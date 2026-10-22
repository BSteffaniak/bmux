//! Exclusive, compatibility-aware companion startup.
//!
//! Resolution and compatibility policy belong to the caller. This module never
//! discovers executables, selects roots, or removes endpoints. Recovery may stop
//! only an exact owned activation fenced by its execution authority as safe.
//! A successful launch outlives the window; timeout does not authorize killing it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::future::Future;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Fully resolved launch authority. Arguments must select the same complete
/// roots and endpoint inspected by `probe`; a runtime name alone is insufficient.
#[derive(Debug, Clone)]
pub struct RuntimeLaunchSpec {
    /// Absolute, bundle-verified companion executable (never `current_exe`).
    pub executable: PathBuf,
    /// Complete server command, including explicit root selectors.
    pub arguments: Vec<OsString>,
    /// Deliberately resolved child environment. Ambient variables are cleared.
    pub environment: BTreeMap<OsString, OsString>,
    /// Absolute existing working directory, independent of the window's cwd.
    pub working_directory: PathBuf,
    /// Absolute lock file shared by all launchers of this endpoint. Never unlink.
    pub ownership_file: PathBuf,
    /// Absolute diagnostic file; output must not retain the native window's pipes.
    pub diagnostic_file: PathBuf,
    /// Overall bound, including lock contention and readiness.
    pub timeout: Duration,
    pub poll_interval: Duration,
}

/// Only a missing endpoint or stopped endpoint with a validated ownership receipt
/// permits launch. A held server claim is `Starting`; uncertain/authentication,
/// corrupt metadata and protocol errors remain errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeProbe {
    Absent,
    /// A server process holds the lifecycle claim but has not negotiated readiness.
    Starting,
    Compatible,
    Incompatible(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLaunchOutcome {
    AlreadyRunning,
    Started { pid: u32 },
}

/// Connect to C's resolved selection, starting only its verified managed companion.
///
/// Uses BMUX's existing durable principal and negotiated IPC handshake. External
/// selections never launch. Only a missing endpoint is absent; refused, busy,
/// denied, stale, corrupt or incompatible endpoints are errors, never replacement
/// authority. All child BMUX selectors are rebuilt from the explicit roots.
///
/// # Errors
/// Any uncertain endpoint, compatibility, configuration, ownership or readiness
/// failure is returned without consulting another runtime or killing a process.
pub async fn connect_selection(
    selection: &crate::runtime_selection::RuntimeSelection,
) -> Result<bmux_client::BmuxClient> {
    use crate::runtime_selection::RuntimeSelection;
    let roots = selection.roots();
    let paths = roots.config_paths();
    let timeout = Duration::from_secs(5);
    match selection {
        RuntimeSelection::External { .. } => {
            let mut client = connect_at_paths(&paths, timeout)
                .await
                .context("attach to explicit external runtime")?;
            let readiness = tokio::time::timeout(
                timeout,
                bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(
                    &mut client,
                ),
            )
            .await
            .context("external runtime readiness timed out")??;
            anyhow::ensure!(
                readiness.helper_ready && !readiness.fenced,
                "external runtime is not ready: {}; attach-only selection never repairs a runtime",
                readiness
                    .helper_error
                    .as_deref()
                    .unwrap_or("runtime is fenced")
            );
            return Ok(client);
        }
        RuntimeSelection::Managed { companion, .. } => {
            // Reverify immediately before launch admission rather than trusting an
            // earlier chooser's stale hash. No size/mtime identity is accepted.
            let verified =
                crate::compatibility::VerifiedCompanion::load(companion.executable.clone())?;
            if verified.sha256 != companion.sha256 || verified.bundle_id != companion.bundle_id {
                bail!("companion bundle changed after runtime selection");
            }
        }
    }
    for path in [
        &roots.config_dir,
        &roots.runtime_dir,
        &roots.data_dir,
        &roots.state_dir,
    ] {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create selected root {}", path.display()))?;
    }
    let RuntimeSelection::Managed { companion, .. } = selection else {
        unreachable!()
    };
    let mut environment: BTreeMap<OsString, OsString> = std::env::vars_os()
        .filter(|(key, _)| !key.to_string_lossy().starts_with("BMUX_"))
        .collect();
    for (key, value) in [
        ("BMUX_CONFIG_DIR", &roots.config_dir),
        ("BMUX_RUNTIME_DIR", &roots.runtime_dir),
        ("BMUX_DATA_DIR", &roots.data_dir),
        ("BMUX_STATE_DIR", &roots.state_dir),
    ] {
        environment.insert(key.into(), value.as_os_str().to_owned());
    }
    let spec = RuntimeLaunchSpec {
        executable: companion.executable.clone(),
        arguments: ["server", "start", "--foreground-internal"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        environment,
        working_directory: roots.data_dir.clone(),
        ownership_file: roots.runtime_dir.join("companion-startup.lock"),
        diagnostic_file: roots.runtime_dir.join("companion-startup.log"),
        timeout: Duration::from_secs(20),
        poll_interval: Duration::from_millis(25),
    };
    // One reconciler per persistent runtime, independent of build directories.
    let reconcile_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(roots.runtime_dir.join("companion-reconcile.lock"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match reconcile_lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await
            }
            Err(error) => bail!("runtime reconciliation lock unavailable: {error}"),
        }
    }
    let _legacy_claim = guard_legacy_managed_runtime(roots).await?;
    let mut repaired = false;
    loop {
        let outcome = ensure_runtime(&spec, || probe_paths(&paths, timeout)).await?;
        let mut client = connect_at_paths(&paths, timeout)
            .await
            .context("connect selected companion for readiness")?;
        let readiness = tokio::time::timeout(
            timeout,
            bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(
                &mut client,
            ),
        )
        .await
        .context("runtime readiness timed out")?
        .with_context(|| readiness_failure_context(outcome, &spec))?;
        let receipt_path = roots.runtime_dir.join("companion-activation.json");
        let receipt = serde_json::json!({
            "schema_version": 1, "instance": readiness.instance,
            "executable_sha256": readiness.executable_sha256,
        });
        let owned = if matches!(outcome, RuntimeLaunchOutcome::Started { pid } if pid == readiness.process_id)
            && readiness.executable_sha256.as_deref() == Some(companion.sha256.as_str())
        {
            let temporary = roots.runtime_dir.join("companion-activation.next");
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            use std::io::Write as _;
            file.write_all(&serde_json::to_vec(&receipt)?)?;
            file.sync_all()?;
            std::fs::rename(temporary, &receipt_path)?;
            true
        } else {
            match std::fs::read(&receipt_path) {
                Ok(bytes) => {
                    serde_json::from_slice::<serde_json::Value>(&bytes)
                        .context("corrupt managed activation receipt")?
                        == receipt
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            }
        };
        let identity = tokio::time::timeout(timeout, client.whoami_principal())
            .await
            .context("runtime identity timed out")??;
        let roots_match = readiness.runtime_dir == roots.runtime_dir.to_string_lossy()
            && readiness.config_dir == roots.config_dir.to_string_lossy()
            && readiness.data_dir == roots.data_dir.to_string_lossy()
            && readiness.state_dir == roots.state_dir.to_string_lossy();
        let observation = if readiness.executable_sha256.is_none() {
            crate::reconciliation::Observation::Unknown(
                "runtime executable identity is unavailable".into(),
            )
        } else if !roots_match {
            crate::reconciliation::Observation::Unknown("runtime reported different roots".into())
        } else if readiness.helper_ready && !readiness.fenced {
            crate::reconciliation::Observation::Ready
        } else {
            crate::reconciliation::Observation::Unhealthy {
                owned: owned && identity.principal_id == identity.server_control_principal_id,
                restart_blocker: readiness.restart_blocker.clone(),
            }
        };
        match crate::reconciliation::plan(observation, repaired) {
            crate::reconciliation::Action::Attach => return Ok(client),
            crate::reconciliation::Action::FenceAndRestart => {
                let fence = tokio::time::timeout(timeout,
                    bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::fence_unhealthy(&mut client, readiness.instance)
                ).await.context("runtime fencing timed out")??.map_err(anyhow::Error::msg)?;
                anyhow::ensure!(
                    fence.instance == readiness.instance && fence.fenced && !fence.helper_ready,
                    "runtime did not acknowledge exact-instance recovery fence"
                );
                // Recheck the same connection's authoritative incarnation immediately before shutdown.
                let current = tokio::time::timeout(timeout,
                    bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(&mut client)
                ).await.context("fenced runtime readiness timed out")??;
                anyhow::ensure!(
                    current.instance == fence.instance && current.fenced,
                    "runtime incarnation changed after fencing"
                );
                let stopped = tokio::time::timeout(
                    timeout,
                    client.request_raw(bmux_ipc::Request::ServerStop),
                )
                .await
                .context("graceful recovery stop request timed out")??;
                anyhow::ensure!(
                    matches!(
                        stopped,
                        bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ServerStopping)
                    ),
                    "runtime did not acknowledge graceful shutdown"
                );
                drop(client);
                repaired = true;
                let stop_deadline = Instant::now() + timeout;
                loop {
                    if matches!(probe_paths(&paths, timeout).await?, RuntimeProbe::Absent) {
                        break;
                    }
                    anyhow::ensure!(
                        Instant::now() < stop_deadline,
                        "graceful recovery shutdown timed out; no replacement launched"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            action => bail!(
                "runtime is not ready: {}; reconciliation: {action:?}; runtime preserved",
                readiness
                    .helper_error
                    .as_deref()
                    .unwrap_or("execution readiness unavailable")
            ),
        }
    }
}

async fn connect_at_paths(
    paths: &bmux_config::ConfigPaths,
    timeout: Duration,
) -> Result<bmux_client::BmuxClient> {
    // The existing identity loader uses read-then-write. Serialize native callers
    // so two first windows cannot overwrite the durable principal concurrently.
    std::fs::create_dir_all(&paths.data_dir).context("create selected identity directory")?;
    let identity_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.data_dir.join("bterm-principal.lock"))
        .context("open selected identity lock")?;
    let deadline = Instant::now() + timeout;
    loop {
        match identity_lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    bail!("selected identity lock timed out");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("lock selected identity");
            }
        }
    }
    let mut client = tokio::time::timeout(
        timeout,
        bmux_client::BmuxClient::connect_with_paths_and_timeout(paths, "bterm", timeout),
    )
    .await
    .context("selected runtime handshake timed out")??;
    anyhow::ensure!(
        client.negotiated_protocol().is_some(),
        "selected runtime did not negotiate IPC compatibility"
    );
    client
        .require_service_contracts()
        .context("selected runtime lacks checked plugin service compatibility")?;
    Ok(client)
}

fn readiness_failure_context(outcome: RuntimeLaunchOutcome, spec: &RuntimeLaunchSpec) -> String {
    let connection = match outcome {
        RuntimeLaunchOutcome::AlreadyRunning => {
            "reused an already-running daemon; rebuilding the bundle does not upgrade that process"
        }
        RuntimeLaunchOutcome::Started { .. } => {
            "launched a new companion; its required plugin readiness service is unavailable"
        }
    };
    format!(
        "runtime lacks authoritative readiness: {connection}; preserved without restart. \
         Selected companion: {}. Selected runtime startup diagnostics: {}. \
         Inspect the selected daemon's plugin registration/activation errors; \
         if it is a legacy daemon, explicitly stop only this selected runtime before retrying",
        spec.executable.display(),
        spec.diagnostic_file.display(),
    )
}

/// Do not bypass a live pre-boot-namespace companion sharing the durable roots.
/// Keep its existing lifetime claim locked through startup admission. Stale files
/// are preserved: a device mismatch here is not permission to unlink anything.
async fn guard_legacy_managed_runtime(
    roots: &crate::runtime_paths::RuntimeRoots,
) -> Result<Option<std::fs::File>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
        let root = roots.config_dir.parent().context("missing managed root")?;
        let legacy = root.join("runtime");
        if roots.runtime_dir == legacy {
            return Ok(None);
        }
        let claim_path = legacy.join("server-lifetime.lock");
        let claim = match OpenOptions::new().read(true).write(true).open(&claim_path) {
            Ok(file) => {
                file.try_lock().context(
                    "legacy managed runtime is running or starting; explicitly stop it before migrating",
                )?;
                let named = std::fs::symlink_metadata(&claim_path)?;
                let opened = file.metadata()?;
                anyhow::ensure!(
                    named.is_file() && named.dev() == opened.dev() && named.ino() == opened.ino(),
                    "legacy lifetime claim was replaced"
                );
                use std::io::{Read as _, Seek as _};
                anyhow::ensure!(opened.len() <= 128, "oversized legacy ownership receipt");
                let mut receipt = String::new();
                let mut reader = &file;
                reader.rewind()?;
                reader.read_to_string(&mut receipt)?;
                if !receipt.is_empty() {
                    let parts: Vec<_> = receipt.split_whitespace().collect();
                    anyhow::ensure!(
                        parts.len() == 3
                            && parts[0] == "bmux-endpoint-v1"
                            && parts[1].parse::<u64>().is_ok()
                            && parts[2].parse::<u64>().is_ok()
                            && receipt == format!("bmux-endpoint-v1 {} {}\n", parts[1], parts[2]),
                        "unsupported or corrupt legacy ownership receipt"
                    );
                }
                Some(file)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("inspect legacy lifetime claim"),
        };
        let socket = legacy.join("server.sock");
        match std::fs::symlink_metadata(&socket) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(claim),
            Err(error) => return Err(error).context("inspect legacy endpoint"),
            Ok(metadata) => anyhow::ensure!(
                metadata.file_type().is_socket() && claim.is_some(),
                "legacy endpoint has uncertain ownership; refusing migration"
            ),
        }
        match tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::UnixStream::connect(socket),
        )
        .await
        {
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => Ok(claim),
            _ => bail!("legacy managed endpoint is live or uncertain; refusing migration"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = roots;
        Ok(None)
    }
}

fn runtime_incompatibility(error: &anyhow::Error) -> Option<String> {
    match error.downcast_ref::<bmux_client::ClientError>()? {
        bmux_client::ClientError::ProtocolIncompatible { reason } => Some(format!("{reason:?}")),
        bmux_client::ClientError::ServerError {
            code:
                bmux_ipc::ErrorCode::ServiceIncompatible
                | bmux_ipc::ErrorCode::UnsupportedRepresentation,
            message,
        } => Some(message.clone()),
        _ => None,
    }
}

async fn probe_paths(paths: &bmux_config::ConfigPaths, timeout: Duration) -> Result<RuntimeProbe> {
    match connect_at_paths(paths, timeout).await {
        Ok(_) => Ok(RuntimeProbe::Compatible),
        Err(error) => {
            if let Some(reason) = runtime_incompatibility(&error) {
                return Ok(RuntimeProbe::Incompatible(reason));
            }
            // A held claim means starting only for transport-level unavailability.
            // Authentication/protocol/application failures remain explicit errors.
            let unavailable = matches!(error.downcast_ref::<bmux_client::ClientError>(),
                Some(bmux_client::ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(io)))
                if matches!(io.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::WouldBlock)
                    || io.raw_os_error() == Some(231));
            if !unavailable {
                return Err(error)
                    .context("selected endpoint state is uncertain; refusing replacement");
            }
            let claim_path = paths.runtime_dir.join("server-lifetime.lock");
            if let Ok(metadata) = std::fs::symlink_metadata(&claim_path) {
                anyhow::ensure!(
                    metadata.file_type().is_file(),
                    "runtime lifetime claim is not a regular file"
                );
            }
            let claim = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&claim_path)
                .context("inspect selected runtime lifetime claim")?;
            match claim.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Ok(RuntimeProbe::Starting),
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error).context("inspect selected lifetime ownership");
                }
            }
            #[cfg(unix)]
            if let Some(state) = classify_stopped_unix(paths, &claim_path, claim, &error)? {
                return Ok(state);
            }
            #[cfg(windows)]
            anyhow::ensure!(
                claim.metadata()?.len() == 0,
                "unsupported Windows lifetime claim contents"
            );
            #[cfg(windows)]
            if let Some(bmux_client::ClientError::Transport(
                bmux_ipc::transport::IpcTransportError::Io(io),
            )) = error.downcast_ref::<bmux_client::ClientError>()
                && io.kind() == std::io::ErrorKind::NotFound
            {
                return Ok(RuntimeProbe::Absent);
            }
            Err(error).context("selected endpoint state is uncertain; refusing replacement")
        }
    }
}

#[cfg(unix)]
fn classify_stopped_unix(
    paths: &bmux_config::ConfigPaths,
    claim_path: &std::path::Path,
    claim: std::fs::File,
    error: &anyhow::Error,
) -> Result<Option<RuntimeProbe>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let mut claim = claim;
    let named = std::fs::symlink_metadata(claim_path)?;
    let opened = claim.metadata()?;
    anyhow::ensure!(
        named.file_type().is_file() && named.dev() == opened.dev() && named.ino() == opened.ino(),
        "runtime lifetime claim was replaced"
    );
    anyhow::ensure!(
        claim.metadata()?.len() <= 128,
        "oversized endpoint ownership receipt"
    );
    let mut receipt = String::new();
    claim.seek(SeekFrom::Start(0))?;
    claim.read_to_string(&mut receipt)?;
    if !receipt.is_empty() {
        let mut parts = receipt.split_whitespace();
        anyhow::ensure!(
            parts.next() == Some("bmux-endpoint-v1"),
            "unsupported endpoint ownership receipt"
        );
        let device = parts
            .next()
            .context("corrupt endpoint ownership receipt")?
            .parse::<u64>()?;
        let inode = parts
            .next()
            .context("corrupt endpoint ownership receipt")?
            .parse::<u64>()?;
        anyhow::ensure!(
            parts.next().is_none() && receipt == format!("bmux-endpoint-v1 {device} {inode}\n"),
            "corrupt endpoint ownership receipt"
        );
    }
    let transport_error = error.downcast_ref::<bmux_client::ClientError>();
    let kind = match transport_error {
        Some(bmux_client::ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(
            io,
        ))) => Some(io.kind()),
        _ => None,
    };
    match std::fs::symlink_metadata(paths.server_socket()) {
        Err(io)
            if io.kind() == std::io::ErrorKind::NotFound
                && kind == Some(std::io::ErrorKind::NotFound) =>
        {
            return Ok(Some(RuntimeProbe::Absent));
        }
        Ok(metadata)
            if metadata.file_type().is_socket()
                && kind == Some(std::io::ErrorKind::ConnectionRefused) =>
        {
            let expected = format!("bmux-endpoint-v1 {} {}\n", metadata.dev(), metadata.ino());
            anyhow::ensure!(
                receipt == expected,
                "stopped endpoint has no matching ownership receipt; refusing replacement"
            );
            // The server will reacquire this claim and revalidate before
            // reclaiming. The frontend never removes endpoint names.
            return Ok(Some(RuntimeProbe::Absent));
        }
        _ => {}
    }
    Ok(None)
}

/// Inspect, exclusively launch if absent, then wait for compatible readiness.
///
/// The probe must perform an authenticated compatibility handshake, not merely
/// connect to a socket. Each probe is bounded by the remaining overall timeout.
/// No incompatible instance is stopped or replaced. The caller owns endpoint
/// recovery; this function never interprets connection refusal as permission to
/// remove a socket. OS locks are released on crash and remain held through readiness.
///
/// # Errors
/// Invalid paths, uncertain/incompatible probes, lock/spawn failures, early child
/// exit and readiness timeout are reported without fallback to another runtime.
pub async fn ensure_runtime<P, F>(
    spec: &RuntimeLaunchSpec,
    mut probe: P,
) -> Result<RuntimeLaunchOutcome>
where
    P: FnMut() -> F,
    F: Future<Output = Result<RuntimeProbe>>,
{
    validate(spec)?;
    let deadline = Instant::now()
        .checked_add(spec.timeout)
        .context("invalid startup timeout")?;
    loop {
        let observed = match probe_until(deadline, probe()).await? {
            RuntimeProbe::Compatible => crate::reconciliation::Observation::Ready,
            RuntimeProbe::Incompatible(reason) => {
                crate::reconciliation::Observation::Unknown(reason)
            }
            RuntimeProbe::Starting => crate::reconciliation::Observation::Starting,
            RuntimeProbe::Absent => crate::reconciliation::Observation::Absent,
        };
        match crate::reconciliation::plan(observed, false) {
            crate::reconciliation::Action::Attach => {
                return Ok(RuntimeLaunchOutcome::AlreadyRunning);
            }
            crate::reconciliation::Action::Wait => pause(spec, deadline).await?,
            crate::reconciliation::Action::Start => break,
            action => bail!("selected runtime is incompatible: {action:?}"),
        }
    }
    let ownership = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&spec.ownership_file)
        .context("open runtime startup ownership file")?;
    loop {
        match ownership.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => pause(spec, deadline).await?,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("lock runtime startup");
            }
        }
    }
    // The previous owner may have completed startup while this caller waited.
    loop {
        match probe_until(deadline, probe()).await? {
            RuntimeProbe::Compatible => return Ok(RuntimeLaunchOutcome::AlreadyRunning),
            RuntimeProbe::Incompatible(reason) => {
                bail!("selected runtime is incompatible: {reason}")
            }
            RuntimeProbe::Starting => pause(spec, deadline).await?,
            RuntimeProbe::Absent => break,
        }
    }
    let mut child = PendingChild::spawn(spec, ownership)?;
    let pid = child.process().id();
    let result = loop {
        match child
            .process()
            .try_wait()
            .context("inspect companion startup")?
        {
            Some(status) => {
                // A concurrently started CLI server may have won the lifetime
                // claim after our pre-launch probe. Its readiness is authoritative,
                // not the fact that this window's candidate exited.
                match probe_until(deadline, probe()).await {
                    Ok(RuntimeProbe::Compatible) => break Ok(RuntimeLaunchOutcome::AlreadyRunning),
                    Ok(RuntimeProbe::Starting) => {
                        if let Err(error) = pause(spec, deadline).await {
                            break Err(error);
                        }
                        continue;
                    }
                    Ok(RuntimeProbe::Incompatible(reason)) => {
                        break Err(anyhow::anyhow!(
                            "selected runtime is incompatible: {reason}"
                        ));
                    }
                    Ok(RuntimeProbe::Absent) => {}
                    Err(error) => break Err(error),
                }
                break Err(anyhow::anyhow!(
                    "companion exited before readiness: {status}; diagnostics: {}",
                    spec.diagnostic_file.display()
                ));
            }
            None => match probe_until(deadline, probe()).await {
                Ok(RuntimeProbe::Compatible) => break Ok(RuntimeLaunchOutcome::Started { pid }),
                Ok(RuntimeProbe::Incompatible(reason)) => {
                    break Err(anyhow::anyhow!(
                        "selected runtime is incompatible: {reason}"
                    ));
                }
                Ok(RuntimeProbe::Absent | RuntimeProbe::Starting) => {
                    if let Err(error) = pause(spec, deadline).await {
                        break Err(error);
                    }
                }
                Err(error) => break Err(error),
            },
        }
    };
    if result.is_ok() {
        // Compatible readiness permits another launcher to inspect normally.
        // On error/cancellation keep ownership until this child actually exits.
        drop(child.ownership.take());
    }
    result
}

struct PendingChild {
    child: Option<Child>,
    ownership: Option<std::fs::File>,
    reaper: std::sync::mpsc::Sender<(Child, Option<std::fs::File>)>,
}

impl PendingChild {
    fn spawn(spec: &RuntimeLaunchSpec, ownership: std::fs::File) -> Result<Self> {
        let (sender, receiver) = std::sync::mpsc::channel::<(Child, Option<std::fs::File>)>();
        // Reserve the reaper before spawning. If a thread cannot be created,
        // fail before there is a child whose uncertain startup must stay locked.
        std::thread::Builder::new()
            .name("bmux-companion-reaper".into())
            .spawn(move || {
                if let Ok((mut child, _ownership)) = receiver.recv() {
                    let _ = child.wait();
                }
            })
            .context("reserve companion reaper")?;
        Ok(Self {
            child: Some(spawn_detached(spec)?),
            ownership: Some(ownership),
            reaper: sender,
        })
    }

    fn process(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("pending child exists until drop")
    }
}

impl Drop for PendingChild {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let ownership = self.ownership.take();
        // A timed-out or cancelled startup is uncertain, not absent. Retain the
        // lock while reaping so another window cannot race a still-starting child.
        if let Err(undelivered) = self.reaper.send((child, ownership)) {
            // A lost reaper must not drop ownership of an uncertain live child.
            // Retain its OS lock until this frontend exits rather than guess.
            std::mem::forget(undelivered.0);
        }
    }
}

fn validate(spec: &RuntimeLaunchSpec) -> Result<()> {
    for path in [
        &spec.executable,
        &spec.working_directory,
        &spec.ownership_file,
        &spec.diagnostic_file,
    ] {
        if !path.is_absolute() {
            bail!(
                "companion startup requires absolute paths: {}",
                path.display()
            );
        }
    }
    if spec.timeout.is_zero() || spec.poll_interval.is_zero() {
        bail!("companion startup timeout and poll interval must be positive");
    }
    Ok(())
}

async fn probe_until<F>(deadline: Instant, probe: F) -> Result<RuntimeProbe>
where
    F: Future<Output = Result<RuntimeProbe>>,
{
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("companion readiness timed out")?;
    tokio::time::timeout(remaining, probe)
        .await
        .context("companion readiness timed out")?
}

async fn pause(spec: &RuntimeLaunchSpec, deadline: Instant) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("companion readiness timed out")?;
    tokio::time::sleep(spec.poll_interval.min(remaining)).await;
    Ok(())
}

fn spawn_detached(spec: &RuntimeLaunchSpec) -> Result<Child> {
    let diagnostics = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.diagnostic_file)
        .context("open companion startup diagnostics")?;
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.arguments)
        .env_clear()
        .envs(&spec.environment)
        .current_dir(&spec.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(diagnostics));
    detach(&mut command);
    command.spawn().context("spawn explicit BMUX companion")
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: setsid is async-signal-safe and touches no Rust state after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
compile_error!("BMUX companion startup supports Unix and Windows only");

#[cfg(test)]
mod tests;
