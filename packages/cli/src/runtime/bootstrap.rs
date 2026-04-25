use anyhow::{Context, Result};
use bmux_cli_schema::{LogLevel, RecordingEventKindArg, RecordingProfileArg};
use bmux_client::{BmuxClient, ClientError};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_contexts_plugin_api::contexts_commands::{
    ContextAck as ContextAckRecord, CreateContextError,
};
use bmux_ipc::{RecordingEventKind, RecordingRollingStartOptions};
use bmux_server::BmuxServer;
use bmux_sessions_plugin_api::sessions_state::SessionSummary;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use tracing::{Level, warn};
use uuid::Uuid;

use super::{typed_contexts, typed_sessions};

use super::{
    ConnectionContext, ConnectionPolicyScope, EFFECTIVE_LOG_LEVEL, LOG_WRITER_GUARD,
    SERVER_START_TIMEOUT, activate_loaded_plugins, append_runtime_arg, cleanup_stale_pid_file,
    connect_with_context, deactivate_loaded_plugins, dispatch_loaded_plugin_event,
    load_enabled_plugins, map_client_connect_error, plugin_event_bridge_loop, plugin_system_event,
    recording, register_plugin_service_handlers, remove_server_pid_file, resolve_log_level,
    run_server_stop, run_session_attach_with_client, scan_available_plugins, server_is_running,
    tracing_level, try_kill_pid, validate_enabled_plugins, wait_for_server_running,
    write_server_pid_file, write_server_runtime_metadata,
};

#[derive(Debug, Clone)]
pub(super) struct DefaultAttachOptions {
    pub(super) record: bool,
    pub(super) capture_input: bool,
    pub(super) profile: Option<RecordingProfileArg>,
    pub(super) name: Option<String>,
    pub(super) event_kinds: Vec<RecordingEventKindArg>,
    pub(super) recording_id_file: Option<String>,
    pub(super) stop_server_on_exit: bool,
}

pub(super) async fn run_default_server_attach(
    options: DefaultAttachOptions,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    if options.record {
        ensure_server_not_running_for_record_bootstrap(connection_context).await?;
    }
    ensure_server_running_for_default_attach(connection_context).await?;

    let mut active_recording_id = None;
    if options.record {
        let mut recording_client = connect_with_context(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-start",
            connection_context,
        )
        .await?;
        let started = bmux_recording_plugin_api::typed_client::recording_start(
            &mut recording_client,
            None,
            options.capture_input,
            options.name.clone(),
            recording::recording_profile_arg_to_ipc(options.profile),
            recording::resolve_event_kind_override(
                options.profile,
                &options.event_kinds,
                options.capture_input,
            ),
        )
        .await?;
        active_recording_id = Some(started.id);
        let name_display = started.name.as_deref().unwrap_or("-");
        println!(
            "recording started: {} name={} (capture_input={})",
            started.id, name_display, started.capture_input
        );
        if let Some(path) = options.recording_id_file.as_deref() {
            std::fs::write(path, format!("{}\n", started.id))
                .with_context(|| format!("failed writing recording id file {path}"))?;
        }
    }

    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-default-attach",
        connection_context,
    )
    .await?;
    let target = resolve_default_attach_target(&mut client).await?;
    let target = target.to_string();
    let attach_result =
        run_session_attach_with_client(client, Some(target.as_str()), None, false, None)
            .await
            .map(|outcome| outcome.status_code);

    if let Some(recording_id) = active_recording_id {
        let mut stop_client = connect_with_context(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-stop",
            connection_context,
        )
        .await?;
        let stopped_id = bmux_recording_plugin_api::typed_client::recording_stop(
            &mut stop_client,
            Some(recording_id),
        )
        .await
        .with_context(|| format!("failed stopping recording {recording_id}"))?;
        let mut list_client = connect_with_context(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-list",
            connection_context,
        )
        .await?;
        let recording = bmux_recording_plugin_api::typed_client::recording_list(&mut list_client)
            .await?
            .into_iter()
            .find(|summary| summary.id == stopped_id);
        if let Some(recording) = recording {
            let name_display = recording.name.as_deref().unwrap_or("-");
            println!(
                "recording stopped: {} name={} events={} bytes={} path={}",
                recording.id,
                name_display,
                recording.event_count,
                recording.payload_bytes,
                recording.path
            );
            let recording_path = std::path::PathBuf::from(&recording.path);
            recording::maybe_auto_export_recording(stopped_id, Some(&recording_path)).await;
        } else {
            println!("recording stopped: {stopped_id}");
            recording::maybe_auto_export_recording(stopped_id, None).await;
        }
    }

    if options.record && options.stop_server_on_exit {
        let _ = run_server_stop(connection_context).await;
    }

    attach_result
}

pub(super) async fn ensure_server_not_running_for_record_bootstrap(
    connection_context: ConnectionContext<'_>,
) -> Result<()> {
    if server_is_running(connection_context).await? {
        anyhow::bail!(
            "--record requires a fresh start but server is already running; stop it first or run without --record"
        )
    }
    Ok(())
}

pub(super) async fn ensure_server_running_for_default_attach(
    connection_context: ConnectionContext<'_>,
) -> Result<()> {
    if server_is_running(connection_context).await? {
        return Ok(());
    }

    let _ = run_server_start(
        true,
        false,
        None,
        RecordingRollingStartOptions::default(),
        None,
    )
    .await?;
    if !server_is_running(connection_context).await? {
        anyhow::bail!("bmux server failed to start for default attach")
    }
    Ok(())
}

pub(super) async fn resolve_default_attach_target(client: &mut BmuxClient) -> Result<Uuid> {
    let sessions = typed_list_sessions_for_bootstrap(client).await?;

    // Fresh-server path: no sessions means no contexts either. Create
    // a context via contexts-plugin; it allocates a session atomically
    // (contexts own session lifecycle). The returned ack carries the
    // new session id, which is what the attach runtime needs.
    //
    // Going through `create-context` (rather than `new-session` +
    // press-`c`-later) gives the user a ready-to-use tab on first
    // attach with zero UX steps. The alternative — a bare session with
    // no context — landed the user in an empty view and required them
    // to manually create a tab before anything was interactive.
    if sessions.is_empty() {
        let name = next_default_tab_name(&sessions);
        let ack = typed_create_context_for_bootstrap(client, Some(name)).await?;
        let session_id = ack.session_id.ok_or_else(|| {
            anyhow::anyhow!(
                "contexts-plugin create-context returned no session_id; contexts-plugin \
                 is required to allocate a session atomically"
            )
        })?;
        return Ok(session_id);
    }

    let _client_id = bmux_clients_plugin_api::typed_client::whoami(client).await?;
    let writable_sessions = sessions.clone();

    if writable_sessions.is_empty() {
        let name = next_default_tab_name(&sessions);
        let ack = typed_create_context_for_bootstrap(client, Some(name)).await?;
        let session_id = ack.session_id.ok_or_else(|| {
            anyhow::anyhow!(
                "contexts-plugin create-context returned no session_id; contexts-plugin \
                 is required to allocate a session atomically"
            )
        })?;
        return Ok(session_id);
    }

    let mut sorted = writable_sessions;
    sorted.sort_by(|left, right| {
        let left_key = left.name.as_deref().unwrap_or("");
        let right_key = right.name.as_deref().unwrap_or("");
        left_key.cmp(right_key).then_with(|| left.id.cmp(&right.id))
    });

    let session = sorted
        .into_iter()
        .next()
        .expect("non-empty sessions should have first entry");
    Ok(session.id)
}

/// Typed dispatch wrapper for `sessions-state:list-sessions` via
/// `BmuxClient::invoke_service_raw`. Replaces the deprecated
/// `BmuxClient::list_sessions` convenience method that predated the
/// typed plugin contract.
async fn typed_list_sessions_for_bootstrap(client: &mut BmuxClient) -> Result<Vec<SessionSummary>> {
    let payload = bmux_codec::to_vec(&()).context("encoding list-sessions args")?;
    let bytes = client
        .invoke_service_raw(
            typed_sessions::SESSIONS_READ_CAPABILITY.as_str(),
            typed_sessions::QUERY_KIND,
            typed_sessions::SESSIONS_STATE_INTERFACE.as_str(),
            typed_sessions::OP_LIST_SESSIONS,
            payload,
        )
        .await
        .map_err(map_cli_client_error)?;
    bmux_codec::from_bytes::<Vec<SessionSummary>>(&bytes).context("decoding list-sessions response")
}

/// Typed dispatch wrapper for `contexts-commands:create-context`.
///
/// Used by the default-attach bootstrap to atomically create a
/// context-and-session pair on a fresh server. Going through
/// contexts-plugin (which owns context lifecycle) instead of calling
/// sessions-plugin directly ensures the user lands in a usable tab
/// without having to press `c` manually.
async fn typed_create_context_for_bootstrap(
    client: &mut BmuxClient,
    name: Option<String>,
) -> Result<ContextAckRecord> {
    let payload = bmux_codec::to_vec(&typed_contexts::CreateContextArgs {
        name,
        attributes: BTreeMap::new(),
    })
    .context("encoding create-context args")?;
    let bytes = client
        .invoke_service_raw(
            typed_contexts::CONTEXTS_WRITE_CAPABILITY.as_str(),
            typed_contexts::COMMAND_KIND,
            typed_contexts::CONTEXTS_COMMANDS_INTERFACE.as_str(),
            typed_contexts::OP_CREATE_CONTEXT,
            payload,
        )
        .await
        .map_err(map_cli_client_error)?;
    let outcome = bmux_codec::from_bytes::<Result<ContextAckRecord, CreateContextError>>(&bytes)
        .context("decoding create-context response")?;
    outcome.map_err(|err| anyhow::anyhow!("failed to create context: {err:?}"))
}

pub(super) fn next_default_tab_name(sessions: &[SessionSummary]) -> String {
    let mut next = 1_u32;
    loop {
        let candidate = format!("tab-{next}");
        if sessions
            .iter()
            .all(|session| session.name.as_deref() != Some(candidate.as_str()))
        {
            return candidate;
        }
        next = next.saturating_add(1);
    }
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_server_start(
    daemon: bool,
    foreground_internal: bool,
    rolling_enabled_override: Option<bool>,
    rolling_options: RecordingRollingStartOptions,
    pane_shell_integration_override: Option<bool>,
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    if server_is_running(ConnectionContext::default()).await? {
        println!("bmux server is already running");
        return Ok(1);
    }

    let config = BmuxConfig::load()?;
    if let Some(window_secs) = rolling_options.window_secs
        && window_secs == 0
    {
        anyhow::bail!("--rolling-window-secs must be greater than 0")
    }
    let base_rolling_settings = bmux_server::rolling_recording_settings_from_config(&config);
    let effective_rolling_settings =
        bmux_server::apply_rolling_start_options(&base_rolling_settings, &rolling_options);
    let explicit_rolling_event_selection = rolling_options.event_kinds.is_some()
        || rolling_options.capture_input.is_some()
        || rolling_options.capture_output.is_some()
        || rolling_options.capture_events.is_some()
        || rolling_options.capture_protocol_replies.is_some()
        || rolling_options.capture_images.is_some();
    let rolling_requested = rolling_enabled_override == Some(true)
        || rolling_options.window_secs.is_some()
        || explicit_rolling_event_selection
        || (rolling_enabled_override.is_none() && config.recording.enabled);
    if rolling_requested
        && effective_rolling_settings.window_secs == 0
        && (rolling_enabled_override == Some(true)
            || rolling_options.window_secs.is_some()
            || explicit_rolling_event_selection)
    {
        anyhow::bail!(
            "rolling recording was explicitly enabled but window is 0s; set `recording.rolling_window_secs` in config or pass `--rolling-window-secs <secs>`"
        )
    }
    let effective_rolling_enabled = if rolling_enabled_override == Some(false) {
        false
    } else if rolling_options.window_secs.is_some() || explicit_rolling_event_selection {
        true
    } else {
        rolling_enabled_override.unwrap_or(config.recording.enabled)
    } && effective_rolling_settings.window_secs > 0;
    if effective_rolling_enabled && effective_rolling_settings.event_kinds.is_empty() {
        anyhow::bail!(
            "rolling recording is enabled but no rolling event kinds are selected; enable rolling capture flags or pass --rolling-event-kind/--rolling-event-kind-all"
        )
    }
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    validate_enabled_plugins(&config, &registry)?;
    let _preloaded_plugins = load_enabled_plugins(&config, &registry)?;

    if daemon && !foreground_internal {
        let executable =
            std::env::current_exe().context("failed to resolve bmux executable path")?;
        let mut child = ProcessCommand::new(executable);
        append_runtime_arg(&mut child);
        let log_level = EFFECTIVE_LOG_LEVEL.get().copied().unwrap_or(Level::INFO);
        child
            .arg("server")
            .arg("start")
            .arg("--foreground-internal")
            .args(rolling_start_override_args(
                rolling_enabled_override,
                &rolling_options,
                pane_shell_integration_override,
            ))
            .env(
                "BMUX_LOG_LEVEL",
                match log_level {
                    Level::ERROR => "error",
                    Level::WARN => "warn",
                    Level::INFO => "info",
                    Level::DEBUG => "debug",
                    Level::TRACE => "trace",
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = child.spawn().context("failed to spawn background server")?;
        write_server_pid_file(child.id())?;
        write_server_runtime_metadata(child.id())?;

        if !wait_for_server_running(SERVER_START_TIMEOUT, ConnectionContext::default()).await? {
            let _ = try_kill_pid(child.id());
            let _ = remove_server_pid_file();
            anyhow::bail!("background server did not become ready before timeout")
        }

        println!("bmux server started in daemon mode (pid {})", child.id());
        return Ok(0);
    }

    let loaded_plugins = load_enabled_plugins(&config, &registry)?;
    register_recording_plugin_config(
        &config,
        &paths,
        effective_rolling_enabled,
        &effective_rolling_settings,
    );
    register_snapshot_plugin_config(&paths);
    activate_loaded_plugins(&loaded_plugins, &config, &paths)?;
    dispatch_loaded_plugin_event(&loaded_plugins, &plugin_system_event("server_starting"))?;
    let server = BmuxServer::from_config_paths_with_start_options(
        &paths,
        effective_rolling_enabled,
        effective_rolling_settings.window_secs,
        &effective_rolling_settings.event_kinds,
        pane_shell_integration_override,
    );
    register_plugin_service_handlers(&server, &config, &paths, &registry)?;
    // Spawn a plugin-bus → streaming-client forwarder task for every
    // `event_publications` entry that opted into
    // `forward_to_streaming_clients`. These live for the server's
    // lifetime; they terminate automatically when the server drops
    // (see `spawn_plugin_bus_forwarder` for the Weak-held shutdown
    // story).
    for plugin in &loaded_plugins {
        for publication in &plugin.declaration.event_publications {
            if !publication.forward_to_streaming_clients {
                continue;
            }
            let kind = &publication.kind;
            let spawn_result = match (kind.as_str(), publication.delivery) {
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_scene_protocol::scene_protocol::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_scene_protocol::scene_protocol::EventPayload,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_contexts_plugin_api::contexts_events::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_contexts_plugin_api::contexts_events::ContextEvent,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::State)
                    if k == bmux_windows_plugin_api::windows_list::STATE_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_state_forwarder::<
                        bmux_windows_plugin_api::windows_list::WindowListSnapshot,
                    >(kind)
                }
                _ => {
                    tracing::warn!(
                        plugin_id = plugin.declaration.id.as_str(),
                        kind = kind.as_str(),
                        delivery = ?publication.delivery,
                        "no plugin-bus forwarder specialization registered for this kind; \
                         streaming clients will miss emissions until one is added in bootstrap",
                    );
                    continue;
                }
            };
            if let Err(error) = spawn_result {
                tracing::warn!(
                    plugin_id = plugin.declaration.id.as_str(),
                    kind = publication.kind.as_str(),
                    error = %error,
                    "failed to spawn plugin-bus forwarder; streaming clients will miss emissions for this kind",
                );
            }
        }
    }
    write_server_pid_file(std::process::id())?;
    write_server_runtime_metadata(std::process::id())?;
    dispatch_loaded_plugin_event(&loaded_plugins, &plugin_system_event("server_started"))?;
    let run_result = if loaded_plugins.is_empty() {
        server.run().await
    } else {
        let (plugin_bridge_shutdown_tx, plugin_bridge_shutdown_rx) =
            tokio::sync::watch::channel(false);
        let plugin_bridge = plugin_event_bridge_loop(&loaded_plugins, plugin_bridge_shutdown_rx);
        tokio::pin!(plugin_bridge);
        tokio::select! {
            result = server.run() => {
                let _ = plugin_bridge_shutdown_tx.send(true);
                result
            }
            result = &mut plugin_bridge => {
                let _ = plugin_bridge_shutdown_tx.send(true);
                result?;
                Ok(())
            }
        }
    };
    if let Err(error) =
        dispatch_loaded_plugin_event(&loaded_plugins, &plugin_system_event("server_stopping"))
    {
        warn!("failed delivering server_stopping plugin event: {error}");
    }
    if let Err(error) = deactivate_loaded_plugins(&loaded_plugins, &config, &paths) {
        warn!("failed deactivating plugins during server shutdown: {error}");
    }
    let _ = remove_server_pid_file();
    run_result?;
    Ok(0)
}

/// Register the recording plugin's startup config (paths + rolling
/// defaults + auto-start flag) into the plugin state registry. Called
/// between `load_enabled_plugins` and `activate_loaded_plugins` so the
/// recording plugin's `activate` callback can read it and construct
/// its own `RecordingRuntime` instances without depending on
/// `packages/server`.
fn register_recording_plugin_config(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    rolling_auto_start: bool,
    rolling_settings: &bmux_recording_plugin_api::RollingRecordingSettings,
) {
    use bmux_recording_plugin_api::RecordingPluginConfig;
    let plugin_config = RecordingPluginConfig {
        recordings_dir: config.recordings_dir(paths),
        rolling_recordings_dir: paths.rolling_recordings_dir(),
        rolling_segment_mb: config.recording.segment_mb,
        retention_days: config.recording.retention_days,
        rolling_defaults: rolling_settings.clone(),
        rolling_auto_start,
    };
    let handle = std::sync::Arc::new(std::sync::RwLock::new(plugin_config));
    bmux_plugin::global_plugin_state_registry().register::<RecordingPluginConfig>(&handle);
}

/// Register the snapshot plugin's startup config (file path +
/// debounce window) into the plugin state registry. Called between
/// `load_enabled_plugins` and `activate_loaded_plugins` so the
/// snapshot plugin's `activate` callback can read it and construct
/// its orchestrator. The file name is versioned (`bmux-snapshot-v1.json`)
/// so the new combined-envelope format never silently overwrites an
/// older monolithic snapshot.
fn register_snapshot_plugin_config(paths: &ConfigPaths) {
    use bmux_snapshot_plugin_api::SnapshotPluginConfig;
    let plugin_config = SnapshotPluginConfig {
        snapshot_path: paths.data_dir.join("runtime").join("bmux-snapshot-v1.json"),
        debounce_ms: 1_000,
    };
    let handle = std::sync::Arc::new(std::sync::RwLock::new(plugin_config));
    bmux_plugin::global_plugin_state_registry().register::<SnapshotPluginConfig>(&handle);
}

fn rolling_start_override_args(
    rolling_enabled_override: Option<bool>,
    options: &RecordingRollingStartOptions,
    pane_shell_integration_override: Option<bool>,
) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(enabled) = rolling_enabled_override {
        args.push(if enabled {
            "--rolling-recording".to_string()
        } else {
            "--no-rolling-recording".to_string()
        });
    }

    if let Some(window_secs) = options.window_secs {
        args.push("--rolling-window-secs".to_string());
        args.push(window_secs.to_string());
    }

    if let Some(event_kinds) = options.event_kinds.as_deref() {
        for kind in event_kinds {
            args.push("--rolling-event-kind".to_string());
            args.push(recording_event_kind_flag_value(*kind).to_string());
        }
    }

    push_bool_override_flag(
        &mut args,
        options.capture_input,
        "--rolling-capture-input",
        "--no-rolling-capture-input",
    );
    push_bool_override_flag(
        &mut args,
        options.capture_output,
        "--rolling-capture-output",
        "--no-rolling-capture-output",
    );
    push_bool_override_flag(
        &mut args,
        options.capture_events,
        "--rolling-capture-events",
        "--no-rolling-capture-events",
    );
    push_bool_override_flag(
        &mut args,
        options.capture_protocol_replies,
        "--rolling-capture-protocol-replies",
        "--no-rolling-capture-protocol-replies",
    );
    push_bool_override_flag(
        &mut args,
        options.capture_images,
        "--rolling-capture-images",
        "--no-rolling-capture-images",
    );
    push_bool_override_flag(
        &mut args,
        pane_shell_integration_override,
        "--pane-shell-integration",
        "--no-pane-shell-integration",
    );

    args
}

fn push_bool_override_flag(
    args: &mut Vec<String>,
    value: Option<bool>,
    positive: &str,
    negative: &str,
) {
    if let Some(value) = value {
        args.push(if value {
            positive.to_string()
        } else {
            negative.to_string()
        });
    }
}

const fn recording_event_kind_flag_value(kind: RecordingEventKind) -> &'static str {
    match kind {
        RecordingEventKind::PaneInputRaw => "pane-input-raw",
        RecordingEventKind::PaneOutputRaw => "pane-output-raw",
        RecordingEventKind::ProtocolReplyRaw => "protocol-reply-raw",
        RecordingEventKind::PaneImage => "pane-image",
        RecordingEventKind::ServerEvent => "server-event",
        RecordingEventKind::RequestStart => "request-start",
        RecordingEventKind::RequestDone => "request-done",
        RecordingEventKind::RequestError => "request-error",
        RecordingEventKind::Custom => "custom",
    }
}

pub(super) async fn run_session_attach(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-attach",
        connection_context,
    )
    .await?;
    run_session_attach_with_client(client, target, follow, global, None)
        .await
        .map(|outcome| outcome.status_code)
}

pub(super) fn map_attach_client_error(error: ClientError) -> anyhow::Error {
    match error {
        ClientError::ServerError { code, message } => {
            if matches!(code, bmux_ipc::ErrorCode::AlreadyExists) {
                anyhow::anyhow!("attach failed: session already has an active attached client")
            } else {
                anyhow::anyhow!("attach failed: {message}")
            }
        }
        other => map_client_connect_error(other),
    }
}

pub(super) fn map_cli_client_error(error: ClientError) -> anyhow::Error {
    map_client_connect_error(error)
}

pub(super) fn init_logging(verbose: bool, cli_level: Option<LogLevel>, file_only: bool) {
    let level = resolve_log_level(
        verbose,
        cli_level,
        std::env::var("BMUX_LOG_LEVEL").ok().as_deref(),
    );
    let tracing_level = tracing_level(level);
    let _ = EFFECTIVE_LOG_LEVEL.set(tracing_level);

    {
        let paths = resolve_logging_paths();
        let runtime_level = match level {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        };
        let mut log_config = moosicbox_log_runtime::init::InitConfig::new(&paths);
        log_config.default_env_filter = Some(format!("bmux={runtime_level}"));
        log_config.sinks.file = Some(moosicbox_log_runtime::init::FileSinkConfig {
            mode: moosicbox_log_runtime::init::FileMode::Exact("bmux.log"),
        });
        // Commands that enter raw terminal mode (attach, connect, join, etc.)
        // must not write tracing output to stderr — it would corrupt the TUI.
        // All log output is still captured in the log file at
        // ~/Library/Logs/bmux/bmux.log (macOS) or ~/.local/state/bmux/logs/
        // (Linux).  Non-raw-mode commands keep stderr for interactive debugging.
        if file_only {
            log_config.sinks.stderr = false;
        }
        match moosicbox_log_runtime::init::init(log_config) {
            Ok(handle) => {
                let _ = LOG_WRITER_GUARD.set(handle);
            }
            Err(error) => {
                eprintln!("bmux warning: failed to initialize file logging: {error}");
            }
        }
    }
}

fn resolve_logging_paths() -> moosicbox_log_runtime::LogRuntimePaths {
    let mut paths =
        moosicbox_log_runtime::resolve_paths(&moosicbox_log_runtime::LogRuntimePathsConfig {
            app_name: "bmux",
            state_dir_env: "BMUX_STATE_DIR",
            log_dir_env: "BMUX_LOG_DIR",
        });

    if std::env::var_os("BMUX_LOG_DIR").is_none()
        && let Some(state_dir) = std::env::var_os("BMUX_STATE_DIR")
    {
        paths.log_dir = PathBuf::from(state_dir).join("logs");
    }

    paths
}

#[cfg(test)]
mod tests {
    fn empty_cli() -> bmux_cli_schema::Cli {
        bmux_cli_schema::Cli {
            config: None,
            record: false,
            no_capture_input: false,
            recording_id_file: None,
            record_profile: None,
            record_name: None,
            record_event_kind: Vec::new(),
            stop_server_on_exit: false,
            recordings_dir: None,
            recording_auto_export: false,
            no_recording_auto_export: false,
            recording_auto_export_dir: None,
            target: None,
            runtime: None,
            core_builtins_only: false,
            command: None,
            verbose: false,
            log_level: None,
        }
    }

    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::runtime::attach::runtime::{attach_keymap_from_config, initial_attach_status};
    use crate::runtime::cli_parse::validate_record_bootstrap_flags;
    use crate::runtime::session_cli::attach_quit_failure_status;
    use bmux_cli_schema::Command;
    use bmux_client::ClientError;
    use bmux_config::BmuxConfig;
    use bmux_ipc::ErrorCode;
    use bmux_ipc::transport::IpcTransportError;

    #[test]
    fn validate_record_bootstrap_flags_accepts_plain_defaults() {
        let cli = empty_cli();
        assert!(validate_record_bootstrap_flags(&cli).is_ok());
    }

    #[test]
    fn validate_record_bootstrap_flags_rejects_orphaned_record_flags() {
        let mut cli = empty_cli();
        cli.no_capture_input = true;
        let error = validate_record_bootstrap_flags(&cli).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("--no-capture-input requires --record"),
            "unexpected error: {error}"
        );

        let mut cli = empty_cli();
        cli.record_name = Some("demo".to_string());
        let error = validate_record_bootstrap_flags(&cli).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("--record-name requires --record"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validate_record_bootstrap_flags_rejects_record_with_subcommand() {
        let mut cli = empty_cli();
        cli.record = true;
        cli.command = Some(Command::ListSessions { json: false });
        let error = validate_record_bootstrap_flags(&cli).expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("--record is only supported for top-level interactive start"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn map_attach_client_error_formats_busy_session() {
        let error = map_attach_client_error(ClientError::ServerError {
            code: ErrorCode::AlreadyExists,
            message: "session busy".to_string(),
        });
        assert!(
            error
                .to_string()
                .contains("session already has an active attached client")
        );
    }

    #[test]
    fn map_cli_client_error_formats_transport_not_found() {
        let error = map_cli_client_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )));
        let message = error.to_string();

        assert!(message.contains("bmux server is not running"));
        assert!(message.contains("bmux server start --daemon"));
        assert!(message.contains("XDG_RUNTIME_DIR"));
        assert!(message.contains("TMPDIR"));
    }

    #[test]
    fn map_cli_client_error_keeps_non_not_found_errors() {
        let error = map_cli_client_error(ClientError::Transport(IpcTransportError::Io(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        )));
        let message = error.to_string();

        assert!(message.contains("transport error"));
        assert!(!message.contains("bmux server is not running"));
    }

    #[test]
    fn attach_quit_failure_status_is_actionable_for_policy_errors() {
        let status = attach_quit_failure_status(&ClientError::ServerError {
            code: ErrorCode::InvalidRequest,
            message: "session policy denied for this operation".to_string(),
        });

        assert_eq!(status, "quit blocked by session policy");
    }

    #[test]
    fn initial_attach_status_mentions_help_and_typing() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let status = initial_attach_status(&keymap, "normal", true);
        assert!(status.contains("help"));
        assert!(status.contains("modal input enabled"));
    }
}
