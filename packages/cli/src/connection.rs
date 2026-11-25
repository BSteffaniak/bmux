//! Connection-target resolution for CLI commands.
//!
//! A command reaches the local server by default. A selected target (the
//! `--target` override, `BMUX_TARGET`, or `connections.default_target`) is
//! handed to the transport connectors plugins registered in this process;
//! core never parses a connector's targets.

use anyhow::{Context, Result};
use bmux_client::{
    AttachEndpointConnectFuture, AttachEndpointConnector, BmuxClient, ClientError,
    TransportConnectError, TransportConnector, TransportConnectorRegistry,
    TransportConnectorResolutionError,
};
use bmux_config::{BmuxConfig, ConfigPaths, ConnectionTransport, StaleBuildAction};
use bmux_ipc::IncompatibilityReason;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ServerRuntimeMetadata {
    pub pid: u32,
    pub version: String,
    pub build_id: String,
    pub executable_path: String,
    pub started_at_epoch_ms: u64,
    /// Active slot name, when the server was started under a slot-aware CLI.
    /// Optional for backwards compatibility with pre-slots server-meta files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPolicyScope {
    Normal,
    RecoveryInspection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerBuildPolicyEffect {
    Warn(String),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectionContext<'a> {
    pub target_override: Option<&'a str>,
}

impl<'a> ConnectionContext<'a> {
    pub const fn new(target_override: Option<&'a str>) -> Self {
        Self { target_override }
    }
}

/// This CLI process's transport-connector registry. Registration needs no
/// server, socket or runtime directory, so daemonless clients use it too.
pub fn host_transport_connectors() -> Arc<TransportConnectorRegistry> {
    static REGISTRY: OnceLock<Arc<TransportConnectorRegistry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(TransportConnectorRegistry::new())))
}

/// Keep a plugin-supplied connector registered for this process's lifetime.
/// A plugin registers at most one connector; repeated installs are ignored.
#[cfg_attr(
    not(feature = "bundled-plugins"),
    allow(dead_code, reason = "only bundled plugins supply transport connectors")
)]
pub fn register_plugin_transport_connector(
    plugin_id: &str,
    connector: Arc<dyn TransportConnector>,
) {
    static REGISTERED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let Ok(mut registered) = REGISTERED.get_or_init(Mutex::default).lock() else {
        tracing::warn!(plugin_id, "transport connector registration lock poisoned");
        return;
    };
    if registered.insert(plugin_id.to_owned()) {
        host_transport_connectors().register(connector).forget();
    }
}

/// Dials opaque attach endpoints for attach providers (for example cluster
/// attach) through the same connectors as `--target`.
#[derive(Debug, Default)]
pub struct CliAttachEndpointConnector;

impl AttachEndpointConnector for CliAttachEndpointConnector {
    fn connect<'a>(
        &'a self,
        target: &'a str,
        client_name: &'static str,
    ) -> AttachEndpointConnectFuture<'a> {
        Box::pin(async move {
            let config = BmuxConfig::load().map_err(|error| error.to_string())?;
            let selected = resolve_target_reference(&config, &host_transport_connectors(), target)
                .map_err(|error| format!("{error:#}"))?;
            SelectedTarget(selected)
                .connect(client_name)
                .await
                .map_err(|error| format!("{error:#}"))
        })
    }
}

pub async fn connect_with_context(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
    context: ConnectionContext<'_>,
) -> Result<BmuxClient> {
    let selected = SelectedTarget::resolve(context).await?;
    // The stale-build policy compares this CLI with the local server's
    // recorded build; it says nothing about a connector's server.
    if selected.is_local() {
        apply_stale_build_policy(scope)?;
    }
    selected.connect(client_name).await
}

#[cfg(feature = "bundled-plugin-sessions")]
pub async fn connect_if_running_with_context(
    scope: ConnectionPolicyScope,
    client_name: &'static str,
    context: ConnectionContext<'_>,
) -> Result<Option<BmuxClient>> {
    let selected = SelectedTarget::resolve(context).await?;
    if !selected.is_local() {
        return selected.connect(client_name).await.map(Some);
    }
    apply_stale_build_policy(scope)?;
    match BmuxClient::connect_default(client_name).await {
        Ok(client) => Ok(Some(client)),
        Err(error) if is_server_unavailable_client_error(&error) => Ok(None),
        Err(error) => Err(map_client_connect_error(error)),
    }
}

pub async fn connect_raw(client_name: &'static str) -> Result<BmuxClient> {
    connect_raw_with_context(client_name, ConnectionContext::default()).await
}

pub async fn connect_raw_with_context(
    client_name: &'static str,
    context: ConnectionContext<'_>,
) -> Result<BmuxClient> {
    SelectedTarget::resolve(context)
        .await?
        .connect(client_name)
        .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveTarget {
    /// The local server for this runtime.
    Local,
    /// An opaque target owned by exactly one registered transport connector.
    Connector(String),
}

/// A resolved connection target, pinned across a multi-step operation.
pub struct SelectedTarget(ActiveTarget);

/// The pinned target used by the shutdown sequence.
pub type StopTarget = SelectedTarget;

impl SelectedTarget {
    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "shared resolution entry point that callers await; connector resolution may become asynchronous"
    )]
    pub async fn resolve(context: ConnectionContext<'_>) -> Result<Self> {
        let config = BmuxConfig::load().context("failed loading bmux config")?;
        let selected = context
            .target_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .or_else(|| std::env::var("BMUX_TARGET").ok())
            .or_else(|| config.connections.default_target.clone());
        let Some(target) = selected else {
            return Ok(Self(ActiveTarget::Local));
        };
        resolve_target_reference(&config, &host_transport_connectors(), target.trim()).map(Self)
    }

    pub const fn is_local(&self) -> bool {
        matches!(self.0, ActiveTarget::Local)
    }

    pub async fn connect(&self, client_name: &'static str) -> Result<BmuxClient> {
        match &self.0 {
            ActiveTarget::Local => BmuxClient::connect_default(client_name)
                .await
                .map_err(map_client_connect_error),
            ActiveTarget::Connector(target) => host_transport_connectors()
                .connect(target, client_name)
                .await
                .map_err(map_transport_connect_error),
        }
    }
}

/// Resolve one selected target. `local` (or a configured alias whose
/// transport is local) is the local server; anything else must be owned by
/// exactly one registered connector.
fn resolve_target_reference(
    config: &BmuxConfig,
    connectors: &TransportConnectorRegistry,
    target: &str,
) -> Result<ActiveTarget> {
    let target = target.trim();
    if target.is_empty() || target == "local" {
        return Ok(ActiveTarget::Local);
    }
    let configured = config.connections.targets.get(target);
    if configured.is_some_and(|configured| configured.transport == ConnectionTransport::Local) {
        return Ok(ActiveTarget::Local);
    }
    match connectors.resolve(target) {
        Ok(_) => Ok(ActiveTarget::Connector(target.to_owned())),
        Err(error @ TransportConnectorResolutionError::Ambiguous { .. }) => Err(error.into()),
        Err(TransportConnectorResolutionError::NoConnector { .. }) => {
            if configured.is_some() {
                anyhow::bail!(
                    "target '{target}' is configured with a transport bmux no longer dials (ssh, tls or iroh); \
                     remove it from [connections.targets] and connect to a paired host by its saved name"
                );
            }
            anyhow::bail!(
                "no transport connector accepts target '{target}'; check the name, or enable the plugin that provides it"
            )
        }
    }
}

fn map_transport_connect_error(error: TransportConnectError) -> anyhow::Error {
    match error {
        TransportConnectError::Resolution(error) => error.into(),
        TransportConnectError::Connect { target, source } => {
            let message = match &source {
                ClientError::ProtocolIncompatible { reason } => {
                    format_protocol_incompatibility(reason)
                }
                other => other.to_string(),
            };
            anyhow::anyhow!("failed connecting to target '{target}': {message}")
        }
    }
}

pub fn is_server_unavailable_client_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(io_error))
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
    )
}

pub fn map_client_connect_error(error: ClientError) -> anyhow::Error {
    if let ClientError::ProtocolIncompatible { reason } = &error {
        return anyhow::anyhow!(format_protocol_incompatibility(reason));
    }

    if let ClientError::ServerError { code, .. } = &error
        && *code == bmux_ipc::ErrorCode::VersionMismatch
    {
        return anyhow::anyhow!(
            "bmux error: client/server protocol mismatch. Restart with `bmux server stop` and retry."
        );
    }

    if let ClientError::Transport(bmux_ipc::transport::IpcTransportError::FrameDecode(
        bmux_ipc::frame::FrameDecodeError::UnsupportedVersion { .. },
    )) = &error
    {
        return anyhow::anyhow!(
            "bmux error: client/server protocol mismatch. Restart with `bmux server stop` and retry."
        );
    }

    if is_server_unavailable_client_error(&error) {
        return anyhow::anyhow!(
            "bmux server is not running (IPC endpoint unavailable).\nRun `bmux server start --daemon`.\nTroubleshooting: if the server is running in another shell, ensure both processes use the same runtime directory (`XDG_RUNTIME_DIR`/`TMPDIR`). On Unix, a stale socket file can also cause connection refused; remove stale runtime files or run `bmux server stop` and retry."
        );
    }

    if let ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(io_error)) = &error
        && io_error.kind() == std::io::ErrorKind::UnexpectedEof
    {
        let paths = ConfigPaths::default();
        let runtime = std::env::var("BMUX_RUNTIME_NAME")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "default".to_string());
        return anyhow::anyhow!(
            "bmux error: server connection lost unexpectedly.\nThe server may have crashed or been stopped.\nLocal CLI runtime: {runtime}\nLocal CLI IPC endpoint: {}\nCheck `bmux --runtime {runtime} server status` on this machine and host logs on the remote machine.",
            local_ipc_endpoint_label_for_error(&paths)
        );
    }

    anyhow::Error::from(error)
}

fn local_ipc_endpoint_label_for_error(paths: &ConfigPaths) -> String {
    #[cfg(unix)]
    {
        paths.server_socket().display().to_string()
    }
    #[cfg(windows)]
    {
        paths.server_named_pipe()
    }
}

fn format_protocol_incompatibility(reason: &IncompatibilityReason) -> String {
    match reason {
        IncompatibilityReason::WireEpochMismatch { client, server } => {
            format!(
                "bmux error: incompatible IPC wire epoch (client={client}, server={server}). Restart or update the server so both sides share a wire epoch."
            )
        }
        IncompatibilityReason::NoCommonRevision {
            client_min,
            client_max,
            server_min,
            server_max,
        } => format!(
            "bmux error: no overlapping protocol revision (client={client_min}-{client_max}, server={server_min}-{server_max}). Update either side to overlapping revisions."
        ),
        IncompatibilityReason::MissingCoreCapabilities { missing } => format!(
            "bmux error: missing shared core protocol capabilities: {}. Update server/client so core contracts align.",
            missing.join(", ")
        ),
    }
}

pub fn apply_stale_build_policy(scope: ConnectionPolicyScope) -> Result<()> {
    let config = BmuxConfig::load().context("failed loading bmux config")?;
    let metadata = read_server_runtime_metadata().ok().flatten();
    if scope != ConnectionPolicyScope::RecoveryInspection {
        check_cross_slot_mismatch(active_slot_name().as_deref(), metadata.as_ref())?;
    }
    match evaluate_stale_build_policy(
        scope,
        config.behavior.stale_build_action,
        metadata,
        current_cli_build_id().ok(),
    )? {
        Some(ServerBuildPolicyEffect::Warn(message)) => eprintln!("{message}"),
        None => {}
    }
    Ok(())
}

/// Refuse to attach when the client's active slot does not match the server's
/// recorded slot. Returns Ok(()) when either side is slot-unaware (legacy
/// compatibility) or when both sides agree.
fn check_cross_slot_mismatch(
    client_slot: Option<&str>,
    metadata: Option<&ServerRuntimeMetadata>,
) -> Result<()> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    let server_slot = metadata.slot_name.as_deref();
    match (client_slot, server_slot) {
        (Some(client), Some(server)) if client != server => {
            anyhow::bail!(
                "cross-slot attach refused: this CLI is running under slot {client:?} \
                 but the server in this runtime dir was started under slot {server:?}.\n\
                 Run `bmux-{server}` to talk to that server, or stop it first \
                 (`bmux-{server} server stop`) and start a fresh server under slot \
                 {client:?}."
            );
        }
        (Some(client), None) => {
            // Server was started by a slot-unaware (legacy) CLI; when the
            // client is slot-aware this is almost certainly a configuration
            // error (two installs pointing at the same runtime_dir). Hard-
            // refuse with a clear remediation hint.
            anyhow::bail!(
                "cross-slot attach refused: this CLI is running under slot {client:?} \
                 but the existing server in this runtime dir has no slot recorded \
                 (legacy / pre-slots install).\n\
                 Stop the existing server, then run the slot-aware CLI fresh."
            );
        }
        _ => Ok(()),
    }
}

pub fn evaluate_stale_build_policy(
    scope: ConnectionPolicyScope,
    action: StaleBuildAction,
    metadata: Option<ServerRuntimeMetadata>,
    current_build_id: Option<String>,
) -> Result<Option<ServerBuildPolicyEffect>> {
    if scope == ConnectionPolicyScope::RecoveryInspection {
        return Ok(None);
    }
    if action == StaleBuildAction::Ignore {
        return Ok(None);
    }
    let (Some(metadata), Some(current_build_id)) = (metadata, current_build_id) else {
        return Ok(None);
    };
    if metadata.build_id == current_build_id {
        return Ok(None);
    }

    let message = format_stale_build_message(&metadata, &current_build_id, action);
    match action {
        StaleBuildAction::Ignore => Ok(None),
        StaleBuildAction::Error => anyhow::bail!(message),
        StaleBuildAction::Warn => Ok(Some(ServerBuildPolicyEffect::Warn(message))),
    }
}

fn format_stale_build_message(
    metadata: &ServerRuntimeMetadata,
    current_build_id: &str,
    action: StaleBuildAction,
) -> String {
    format!(
        "bmux {}: running server build differs from current CLI build.\nserver build: {} ({})\ncli build: {}\nRestart with `bmux server stop` and retry.",
        match action {
            StaleBuildAction::Ignore => "notice",
            StaleBuildAction::Error => "error",
            StaleBuildAction::Warn => "warning",
        },
        metadata.build_id,
        metadata.executable_path,
        current_build_id,
    )
}

fn server_runtime_metadata_file_path() -> std::path::PathBuf {
    let paths = bmux_config::ConfigPaths::default();
    paths.runtime_dir.join("server-meta.json")
}

pub fn current_cli_build_id() -> Result<String> {
    let executable = std::env::current_exe().context("failed resolving current executable")?;
    let metadata = std::fs::metadata(&executable).with_context(|| {
        format!(
            "failed reading executable metadata {}",
            executable.display()
        )
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0_u128, |duration| duration.as_millis());
    Ok(format!("{}-{modified}", metadata.len()))
}

#[allow(clippy::cast_possible_truncation)] // Epoch millis won't exceed u64 for billions of years
fn current_server_runtime_metadata(pid: u32) -> Result<ServerRuntimeMetadata> {
    let executable = std::env::current_exe().context("failed resolving current executable")?;
    Ok(ServerRuntimeMetadata {
        pid,
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: current_cli_build_id()?,
        executable_path: executable.display().to_string(),
        started_at_epoch_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as u64),
        slot_name: active_slot_name(),
    })
}

/// Return the active slot name if one has been resolved at bootstrap.
fn active_slot_name() -> Option<String> {
    match crate::runtime::slot::active_slot() {
        crate::runtime::slot::ActiveSlotState::Resolved { slot, .. } => Some(slot.name.clone()),
        _ => None,
    }
}

pub fn write_server_runtime_metadata(pid: u32) -> Result<()> {
    let path = server_runtime_metadata_file_path();
    // Before claiming the runtime dir for this slot, verify no live server
    // under a different slot already owns it. This mirrors the client-side
    // cross-slot check (see `check_cross_slot_mismatch`) but protects
    // against the server-startup direction.
    if let Ok(Some(existing)) = read_server_runtime_metadata() {
        let pid_alive = crate::runtime::is_pid_running_crate(existing.pid).unwrap_or(false);
        let client_slot = active_slot_name();
        if let (Some(client), Some(server)) =
            (client_slot.as_deref(), existing.slot_name.as_deref())
            && client != server
            && pid_alive
        {
            anyhow::bail!(
                "refusing to start server under slot {client:?}: another live server \
                 (pid={}) is already running here under slot {server:?}. \
                 Stop it first with `bmux-{server} server stop`.",
                existing.pid,
            );
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating runtime dir {}", parent.display()))?;
    }
    let metadata = current_server_runtime_metadata(pid)?;
    let payload =
        serde_json::to_vec_pretty(&metadata).context("failed encoding server metadata")?;
    std::fs::write(&path, payload)
        .with_context(|| format!("failed writing server metadata file {}", path.display()))
}

pub fn read_server_runtime_metadata() -> Result<Option<ServerRuntimeMetadata>> {
    let path = server_runtime_metadata_file_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed reading server metadata file {}", path.display())
            });
        }
    };
    let metadata = serde_json::from_slice::<ServerRuntimeMetadata>(&bytes).with_context(|| {
        format!(
            "failed parsing server metadata file {}; remove stale file and retry",
            path.display()
        )
    })?;
    Ok(Some(metadata))
}

pub fn remove_server_runtime_metadata_file() -> Result<()> {
    let path = server_runtime_metadata_file_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed removing server metadata file {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveTarget, CliAttachEndpointConnector, ConnectionPolicyScope, ServerBuildPolicyEffect,
        ServerRuntimeMetadata, check_cross_slot_mismatch, evaluate_stale_build_policy,
        host_transport_connectors, is_server_unavailable_client_error, map_client_connect_error,
        map_transport_connect_error, register_plugin_transport_connector, resolve_target_reference,
    };
    use bmux_client::{
        AttachEndpointConnector, ClientError, TransportConnector, TransportConnectorRegistry,
    };
    use bmux_config::{BmuxConfig, StaleBuildAction};
    use bmux_ipc::IncompatibilityReason;
    use bmux_ipc::frame::FrameDecodeError;
    use bmux_ipc::transport::IpcTransportError;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                unsafe { std::env::set_var(self.key, previous) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "bmux-connection-tests-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn attach_endpoint_connector_uses_opaque_connection_target() {
        let connector = CliAttachEndpointConnector;
        let future = connector.connect("missing-attach-target", "bmux-test-attach-connector");
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
            .expect_err("unknown opaque target must fail");
        assert!(error.contains("missing-attach-target"));
    }

    #[test]
    fn stale_build_policy_ignores_mismatch_when_configured() {
        let effect = evaluate_stale_build_policy(
            ConnectionPolicyScope::Normal,
            StaleBuildAction::Ignore,
            Some(ServerRuntimeMetadata {
                pid: 1,
                version: "0.0.1-alpha.0".to_string(),
                build_id: "server-build".to_string(),
                executable_path: "/tmp/bmux-server".to_string(),
                started_at_epoch_ms: 0,
                slot_name: None,
            }),
            Some("cli-build".to_string()),
        )
        .expect("ignore mode should continue without warning");

        assert!(effect.is_none());
    }

    #[test]
    fn stale_build_policy_blocks_when_configured_error() {
        let error = evaluate_stale_build_policy(
            ConnectionPolicyScope::Normal,
            StaleBuildAction::Error,
            Some(ServerRuntimeMetadata {
                pid: 1,
                version: "0.0.1-alpha.0".to_string(),
                build_id: "server-build".to_string(),
                executable_path: "/tmp/bmux-server".to_string(),
                started_at_epoch_ms: 0,
                slot_name: None,
            }),
            Some("cli-build".to_string()),
        )
        .expect_err("expected stale build policy to block");

        let message = error.to_string();
        assert!(message.contains("running server build differs from current CLI build"));
        assert!(message.contains("bmux server stop"));
    }

    #[test]
    fn stale_build_policy_warns_when_configured() {
        let effect = evaluate_stale_build_policy(
            ConnectionPolicyScope::Normal,
            StaleBuildAction::Warn,
            Some(ServerRuntimeMetadata {
                pid: 1,
                version: "0.0.1-alpha.0".to_string(),
                build_id: "server-build".to_string(),
                executable_path: "/tmp/bmux-server".to_string(),
                started_at_epoch_ms: 0,
                slot_name: None,
            }),
            Some("cli-build".to_string()),
        )
        .expect("warn mode should continue");

        assert!(matches!(
            effect,
            Some(ServerBuildPolicyEffect::Warn(message))
                if message.contains("bmux warning") && message.contains("bmux server stop")
        ));
    }

    #[test]
    fn stale_build_policy_skips_recovery_inspection_commands() {
        let effect = evaluate_stale_build_policy(
            ConnectionPolicyScope::RecoveryInspection,
            StaleBuildAction::Error,
            Some(ServerRuntimeMetadata {
                pid: 1,
                version: "0.0.1-alpha.0".to_string(),
                build_id: "server-build".to_string(),
                executable_path: "/tmp/bmux-server".to_string(),
                started_at_epoch_ms: 0,
                slot_name: None,
            }),
            Some("cli-build".to_string()),
        )
        .expect("recovery commands should bypass stale build policy");

        assert!(effect.is_none());
    }

    #[test]
    fn map_client_connect_error_rewrites_protocol_mismatch() {
        let error = map_client_connect_error(ClientError::Transport(
            IpcTransportError::FrameDecode(FrameDecodeError::UnsupportedVersion {
                actual: 1,
                expected: 3,
            }),
        ));

        let message = error.to_string();
        assert!(message.contains("client/server protocol mismatch"));
        assert!(message.contains("bmux server stop"));
    }

    fn metadata_with_slot(slot: Option<&str>) -> ServerRuntimeMetadata {
        ServerRuntimeMetadata {
            pid: 42,
            version: "0.0.1-alpha.0".to_string(),
            build_id: "b".to_string(),
            executable_path: "/tmp/bmux".to_string(),
            started_at_epoch_ms: 0,
            slot_name: slot.map(ToString::to_string),
        }
    }

    #[test]
    fn cross_slot_check_accepts_matching_slots() {
        let md = metadata_with_slot(Some("stable"));
        check_cross_slot_mismatch(Some("stable"), Some(&md)).unwrap();
    }

    #[test]
    fn cross_slot_check_refuses_mismatched_slots() {
        let md = metadata_with_slot(Some("stable"));
        let err = check_cross_slot_mismatch(Some("dev"), Some(&md))
            .expect_err("expected cross-slot refusal");
        let msg = err.to_string();
        assert!(msg.contains("cross-slot attach refused"));
        assert!(msg.contains("\"dev\""));
        assert!(msg.contains("\"stable\""));
    }

    #[test]
    fn cross_slot_check_refuses_slot_client_against_legacy_server() {
        let md = metadata_with_slot(None);
        let err = check_cross_slot_mismatch(Some("dev"), Some(&md)).expect_err("expected refusal");
        let msg = err.to_string();
        assert!(msg.contains("cross-slot attach refused"));
        assert!(msg.contains("legacy"));
    }

    #[test]
    fn cross_slot_check_allows_legacy_client_and_server() {
        check_cross_slot_mismatch(None, None).unwrap();
    }

    #[test]
    fn cross_slot_check_allows_legacy_client_against_slotted_server() {
        // A legacy (slot-unaware) CLI talking to a slot-aware server is
        // intentionally tolerated; otherwise upgrade ordering would break.
        let md = metadata_with_slot(Some("stable"));
        check_cross_slot_mismatch(None, Some(&md)).unwrap();
    }

    #[test]
    fn cross_slot_check_noop_without_metadata() {
        check_cross_slot_mismatch(Some("dev"), None).unwrap();
    }

    #[test]
    fn map_client_connect_error_formats_transport_not_found() {
        let error = map_client_connect_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )));
        let message = error.to_string();

        assert!(message.contains("bmux server is not running"));
        assert!(message.contains("bmux server start --daemon"));
        assert!(message.contains("XDG_RUNTIME_DIR"));
        assert!(message.contains("TMPDIR"));
    }

    #[test]
    fn map_client_connect_error_formats_transport_connection_refused() {
        let error = map_client_connect_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        )));
        let message = error.to_string();

        assert!(message.contains("bmux server is not running"));
        assert!(message.contains("bmux server start --daemon"));
        assert!(message.contains("stale socket"));
    }

    #[test]
    fn map_client_connect_error_formats_protocol_incompatible_reason() {
        let error = map_client_connect_error(ClientError::ProtocolIncompatible {
            reason: IncompatibilityReason::NoCommonRevision {
                client_min: 3,
                client_max: 5,
                server_min: 1,
                server_max: 2,
            },
        });

        let message = error.to_string();
        assert!(message.contains("no overlapping protocol revision"));
        assert!(message.contains("client=3-5"));
        assert!(message.contains("server=1-2"));
    }

    #[test]
    fn server_unavailable_helper_matches_not_found_and_connection_refused() {
        let not_found = ClientError::Transport(IpcTransportError::Io(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        )));
        assert!(is_server_unavailable_client_error(&not_found));

        let refused = ClientError::Transport(IpcTransportError::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        )));
        assert!(is_server_unavailable_client_error(&refused));

        let denied = ClientError::Transport(IpcTransportError::Io(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
        assert!(!is_server_unavailable_client_error(&denied));
    }

    #[test]
    fn runtime_command_handlers_do_not_bypass_connection_module() {
        let runtime_source = include_str!("runtime/mod.rs");
        assert!(!runtime_source.contains("BmuxClient::connect_default"));
    }

    #[derive(Debug)]
    struct AliasConnector(&'static str);

    impl TransportConnector for AliasConnector {
        fn supports(&self, target: &str) -> bool {
            target == self.0
        }

        fn connect<'a>(
            &'a self,
            _target: &'a str,
            _client_name: &'static str,
        ) -> bmux_client::TransportConnectFuture<'a> {
            Box::pin(async { Err(ClientError::UnexpectedResponse("alias connector")) })
        }
    }

    #[test]
    fn local_and_local_aliases_never_consult_connectors() {
        let registry = TransportConnectorRegistry::new();
        let mut config = BmuxConfig::default();
        config.connections.targets.insert(
            "here".into(),
            bmux_config::ConnectionTargetConfig::default(),
        );
        for target in ["", "local", " local ", "here"] {
            assert_eq!(
                resolve_target_reference(&config, &registry, target).unwrap(),
                ActiveTarget::Local
            );
        }
    }

    #[test]
    fn selected_target_uses_the_supporting_connector() {
        let registry = std::sync::Arc::new(TransportConnectorRegistry::new());
        let _desktop = registry.register(std::sync::Arc::new(AliasConnector("desktop")));
        let config = BmuxConfig::default();
        assert_eq!(
            resolve_target_reference(&config, &registry, "desktop").unwrap(),
            ActiveTarget::Connector("desktop".into())
        );
        let error = resolve_target_reference(&config, &registry, "laptop").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no transport connector accepts target 'laptop'"),
            "{error:#}"
        );
    }

    #[test]
    fn ambiguous_connectors_and_removed_transports_are_errors() {
        let registry = std::sync::Arc::new(TransportConnectorRegistry::new());
        let _one = registry.register(std::sync::Arc::new(AliasConnector("desktop")));
        let _two = registry.register(std::sync::Arc::new(AliasConnector("desktop")));
        let mut config = BmuxConfig::default();
        assert!(
            resolve_target_reference(&config, &registry, "desktop")
                .unwrap_err()
                .to_string()
                .contains("more than one transport connector")
        );
        config.connections.targets.insert(
            "old".into(),
            bmux_config::ConnectionTargetConfig {
                transport: bmux_config::ConnectionTransport::Unsupported,
            },
        );
        assert!(
            resolve_target_reference(&config, &registry, "old")
                .unwrap_err()
                .to_string()
                .contains("no longer dials")
        );
    }

    #[test]
    #[serial]
    fn plugin_connector_registration_is_once_per_plugin_and_needs_no_runtime() {
        let runtime_dir = TempDirGuard::new("daemonless");
        let missing = runtime_dir.path().join("never-created");
        let _runtime_guard = EnvVarGuard::set("BMUX_RUNTIME_DIR", &missing);
        register_plugin_transport_connector(
            "test.connections",
            std::sync::Arc::new(AliasConnector("registered-once")),
        );
        register_plugin_transport_connector(
            "test.connections",
            std::sync::Arc::new(AliasConnector("registered-once")),
        );
        assert!(host_transport_connectors().supports("registered-once"));
        assert!(!missing.exists());
    }

    #[test]
    fn connector_failures_name_the_target() {
        let registry = std::sync::Arc::new(TransportConnectorRegistry::new());
        let _desktop = registry.register(std::sync::Arc::new(AliasConnector("desktop")));
        let error = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(registry.connect("desktop", "test"))
            .unwrap_err();
        let message = map_transport_connect_error(error).to_string();
        assert!(
            message.contains("failed connecting to target 'desktop'"),
            "{message}"
        );
    }
}
