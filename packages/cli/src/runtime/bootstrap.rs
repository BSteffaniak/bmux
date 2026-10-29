use crate::connection::CliAttachEndpointConnector;
use anyhow::{Context, Result};
use bmux_cli_schema::LogLevel;
use bmux_client::{BmuxClient, ClientError};
use bmux_clients_plugin_api::clients_state;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_contexts_plugin_api::contexts_commands::{self, ContextAck as ContextAckRecord};
use bmux_diagnostic_log::{DiagnosticLogConfig, DiagnosticLogMode, DiagnosticLogWriter};
use bmux_recording_plugin_api::ManualRecordingStartOptions;
use bmux_recording_protocol::{RecordingEventKind, RecordingRollingStartOptions};
use bmux_server::BmuxServer;
use bmux_sessions_plugin_api::sessions_state::{self, SessionSummary};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use tracing::{Level, warn};
use uuid::Uuid;

const BOOTSTRAP_RECORDING_START_ENV: &str = "BMUX_BOOTSTRAP_RECORDING_START";

use super::plugin_kernel::RuntimeLoggingHandle;
use super::server_commands::prepare_configured_gateway;
use super::{
    ConnectionContext, ConnectionPolicyScope, EFFECTIVE_LOG_LEVEL, LOG_CONTROL,
    SERVER_START_TIMEOUT, activate_loaded_plugins, active_runtime_name, cleanup_stale_pid_file,
    connect_with_context, deactivate_loaded_plugins, dispatch_loaded_plugin_event,
    load_enabled_plugins, map_client_connect_error, plugin_event_bridge_loop, plugin_system_event,
    register_plugin_service_handlers, remove_server_pid_file, resolve_log_level,
    run_session_attach_with_client, scan_available_plugins, server_is_running, tracing_level,
    validate_enabled_plugins, wait_for_server_running, write_server_pid_file,
    write_server_runtime_metadata,
};
use crate::runtime::diagnostics_layer::OperationalDiagnosticsLayer;

#[derive(Debug, Clone, Copy)]
pub(super) struct DefaultAttachOptions;

#[allow(clippy::too_many_lines)] // Default attach bootstrap is a linear lifecycle; splitting would obscure cleanup ordering.
pub(super) async fn run_default_server_attach(
    _options: DefaultAttachOptions,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    tracing::info!("attach.bootstrap.default_start");
    let attach_connection_context = connection_context
        .target_override
        .map_or(ConnectionContext::new(Some("local")), |target| {
            ConnectionContext::new(Some(target))
        });
    ensure_server_running_for_default_attach(attach_connection_context, None).await?;
    tracing::info!("attach.bootstrap.server_ready");

    tracing::info!("attach.bootstrap.connect_start");
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-default-attach",
        attach_connection_context,
    )
    .await?;
    tracing::info!("attach.bootstrap.connect_done");
    let target = resolve_default_attach_target(&mut client).await?;
    tracing::info!(session_id = %target, "attach.bootstrap.target_resolved");
    let target = target.to_string();
    run_session_attach_with_client(client, Some(target.as_str()), None, false, None)
        .await
        .map(|outcome| outcome.status_code)
}

pub(super) async fn ensure_server_running_for_default_attach(
    connection_context: ConnectionContext<'_>,
    startup_recording: Option<ManualRecordingStartOptions>,
) -> Result<()> {
    if server_is_running(connection_context).await? {
        tracing::info!("attach.bootstrap.server_already_running");
        return Ok(());
    }

    tracing::info!("attach.bootstrap.server_start_required");
    let _ = run_server_start(
        true,
        false,
        None,
        RecordingRollingStartOptions::default(),
        None,
        startup_recording,
    )
    .await?;
    if !server_is_running(connection_context).await? {
        anyhow::bail!("bmux server failed to start for default attach")
    }
    tracing::info!("attach.bootstrap.server_started");
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

    let _client_id = clients_state::client::current_client(client)
        .await?
        .map_err(|err| anyhow::anyhow!("clients-state current-client failed: {err:?}"))?;
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
    sessions_state::client::list_sessions(client)
        .await
        .context("sessions-state list-sessions dispatch failed")
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
    contexts_commands::client::create_context(client, name, BTreeMap::new())
        .await
        .context("contexts-commands create-context dispatch failed")?
        .map_err(|err| anyhow::anyhow!("failed to create context: {err:?}"))
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

fn open_server_startup_log(runtime_dir: &std::path::Path) -> Result<std::fs::File> {
    ConfigPaths::local_ipc_policy()
        .prepare_private_directory(runtime_dir, false)
        .with_context(|| {
            format!(
                "failed creating server runtime directory: {}",
                runtime_dir.display()
            )
        })?;
    let startup_log = runtime_dir.join("server-startup.log");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&startup_log) {
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "startup log is not a regular file"
        );
    }
    options.open(&startup_log).with_context(|| {
        format!(
            "failed opening server startup diagnostics: {}",
            startup_log.display()
        )
    })
}

fn startup_diagnostics_tail(path: &std::path::Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return "startup diagnostics unavailable".into();
    };
    let length = file.metadata().map_or(0, |metadata| metadata.len());
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(8192)));
    let mut bytes = Vec::new();
    let _ = file.take(8192).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn server_start_child_arguments(
    rolling_enabled_override: Option<bool>,
    rolling_options: &RecordingRollingStartOptions,
    pane_shell_integration_override: Option<bool>,
) -> Vec<String> {
    let mut arguments = vec![
        "--runtime".to_string(),
        active_runtime_name(),
        "server".to_string(),
        "start".to_string(),
        "--foreground-internal".to_string(),
    ];
    if let Some(path) = bmux_config::ConfigLoadOverrides::for_process().cli_config_path {
        arguments.splice(
            0..0,
            ["--config".to_string(), path.to_string_lossy().into_owned()],
        );
    }
    arguments.extend(rolling_start_override_args(
        rolling_enabled_override,
        rolling_options,
        pane_shell_integration_override,
    ));
    arguments
}

#[cfg(test)]
#[test]
fn server_start_future_size_diagnostic() {
    eprintln!(
        "server_start_inner bytes={}",
        super::dispatch::future_size(|| {
            run_server_start_inner(
                false,
                true,
                None,
                RecordingRollingStartOptions::default(),
                None,
                None,
            )
        })
    );
    eprintln!(
        "plugin_bridge bytes={}",
        super::dispatch::future_size(|| {
            let (_, rx) = tokio::sync::watch::channel(false);
            plugin_event_bridge_loop(std::sync::Arc::new(Vec::new()), rx)
        })
    );
}

// Keep the server lifecycle future off callers stack frames.
pub(super) fn run_server_start(
    daemon: bool,
    foreground_internal: bool,
    rolling_enabled_override: Option<bool>,
    rolling_options: RecordingRollingStartOptions,
    pane_shell_integration_override: Option<bool>,
    startup_recording: Option<ManualRecordingStartOptions>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u8>> + Send>> {
    super::dispatch::startup_future_marker(
        "server_start.before_construct",
        super::dispatch::future_size(|| {
            run_server_start_inner(
                daemon,
                foreground_internal,
                rolling_enabled_override,
                rolling_options.clone(),
                pane_shell_integration_override,
                startup_recording.clone(),
            )
        }),
    );
    Box::pin(run_server_start_inner(
        daemon,
        foreground_internal,
        rolling_enabled_override,
        rolling_options,
        pane_shell_integration_override,
        startup_recording,
    ))
}

#[allow(clippy::too_many_lines)]
async fn run_server_start_inner(
    daemon: bool,
    foreground_internal: bool,
    rolling_enabled_override: Option<bool>,
    rolling_options: RecordingRollingStartOptions,
    pane_shell_integration_override: Option<bool>,
    startup_recording: Option<ManualRecordingStartOptions>,
) -> Result<u8> {
    super::dispatch::startup_future_marker("server_start.first_poll", 0);
    let local_connection_context = ConnectionContext::new(Some("local"));
    let paths = ConfigPaths::default();
    // Retain the launcher's inherited startup fence before any endpoint or
    // plugin work. Dropping closes our copy; never unlock the shared description.
    let _companion_startup_fence =
        super::startup_ownership::inherit_companion_startup_fence(&paths, foreground_internal)?;
    let _managed_executable =
        super::startup_ownership::attest_managed_executable(foreground_internal)?;
    let managed_idle_policy = super::managed_companion::ManagedIdlePolicy::capture()?;
    // Directory preparation can only tighten safe default-path permissions; it
    // never grants the launcher endpoint or lifetime-claim mutation authority.
    paths.ensure_runtime_dir()?;
    #[cfg(unix)]
    if !daemon || foreground_internal {
        let strict = bmux_ipc::transport::UnixSecurityPolicy {
            create_parents: false,
            ..bmux_ipc::transport::UnixSecurityPolicy::strict()
        };
        if let Err(error) = strict.prepare_parent(&paths.server_socket()) {
            tracing::warn!(%error, "local IPC uses private modes and same-user peer authentication, but runtime ancestors permit path replacement; continuing on the selected runtime path");
        }
    }
    // The daemon launcher owns no claim or endpoint mutation authority.
    let startup_ownership = if daemon && !foreground_internal {
        None
    } else {
        let ownership = super::startup_ownership::StartupOwnership::acquire(&paths)?;
        ownership.prepare_endpoint().await?;
        cleanup_stale_pid_file().await?;
        Some(ownership)
    };
    let startup_recording = match startup_recording {
        Some(options) => Some(options),
        None => startup_recording_from_env()?,
    };
    if server_is_running(local_connection_context).await? {
        println!("bmux server is already running");
        return Ok(1);
    }

    let config = BmuxConfig::load()?;
    let configured_gateway = if foreground_internal || !daemon {
        prepare_configured_gateway(&config.server.gateway).await?
    } else {
        None
    };
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
    if daemon && !foreground_internal {
        let executable =
            std::env::current_exe().context("failed to resolve bmux executable path")?;
        let mut child = ProcessCommand::new(executable);
        let log_level = EFFECTIVE_LOG_LEVEL.get().copied().unwrap_or(Level::INFO);
        child
            .args(server_start_child_arguments(
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
            );
        if let Some(startup_recording) = startup_recording.as_ref() {
            set_startup_recording_env(&mut child, startup_recording)?;
        }
        let startup_log = paths.runtime_dir.join("server-startup.log");
        let startup_output = open_server_startup_log(&paths.runtime_dir)?;
        child
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(startup_output));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{
                CREATE_BREAKAWAY_FROM_JOB, DETACHED_PROCESS,
            };
            // Background servers must not inherit a caller-owned SSH job or
            // console lifetime. Disallowed breakaway is an explicit launch
            // failure; do not retry attached to the caller's job.
            child.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        }
        let mut child = child.spawn().context("failed to spawn background server")?;

        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child
                .try_wait()
                .context("failed checking background server")?
            {
                anyhow::bail!(
                    "background server exited with {status}; startup diagnostics: {}\n{}",
                    startup_log.display(),
                    startup_diagnostics_tail(&startup_log)
                );
            }
            if wait_for_server_running(
                std::time::Duration::from_millis(100),
                local_connection_context,
            )
            .await?
            {
                break;
            }
            if started.elapsed() >= SERVER_START_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "background server did not become ready before timeout; startup diagnostics: {}\n{}",
                    startup_log.display(),
                    startup_diagnostics_tail(&startup_log)
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        println!("bmux server started in daemon mode (pid {})", child.id());
        return Ok(0);
    }

    let registry = scan_available_plugins(&config, &paths)?;
    validate_enabled_plugins(&config, &registry)?;
    let loaded_plugins = std::sync::Arc::new(load_enabled_plugins(&config, &registry)?);
    register_recording_plugin_config(
        &config,
        &paths,
        effective_rolling_enabled,
        &effective_rolling_settings,
        startup_recording,
    );
    register_snapshot_plugin_config(&config, &paths);
    register_pane_runtime_plugin_config(&config, &paths, pane_shell_integration_override);
    super::dispatch::startup_future_marker("plugins.before_activate", 0);
    Box::pin(activate_loaded_plugins(&loaded_plugins, &config, &paths)).await?;
    super::dispatch::startup_future_marker("plugins.after_activate", 0);
    dispatch_loaded_plugin_event(&loaded_plugins, &plugin_system_event("server_starting"))?;
    let server = BmuxServer::from_config_paths_with_start_options(
        &paths,
        effective_rolling_enabled,
        effective_rolling_settings.window_secs,
        &effective_rolling_settings.event_kinds,
        pane_shell_integration_override,
    );
    register_plugin_service_handlers(&server, &config, &paths, &registry)?;
    {
        use bmux_plugin::stream_admission::{
            STREAM_ADMISSION_PROVIDER_EXTENSION, STREAM_ADMITTED_ROUTES_EXTENSION,
            StreamAdmissionProviderDeclaration, StreamAdmittedRoute,
        };
        let mut selected = None;
        for plugin in loaded_plugins.iter() {
            for contribution in plugin.collect_activation_contributions()? {
                if let bmux_plugin_sdk::PluginContribution::Extension {
                    extension_point,
                    payload,
                    ..
                } = &contribution
                    && extension_point == STREAM_ADMITTED_ROUTES_EXTENSION
                {
                    let routes: Vec<StreamAdmittedRoute> =
                        bmux_plugin_sdk::decode_service_message(payload)?;
                    for route in routes {
                        anyhow::ensure!(
                            !route.operation.is_empty()
                                && route.operation != "*"
                                && plugin.declaration.services.iter().any(|service| service
                                    .capability
                                    .as_str()
                                    == route.capability
                                    && service.interface_id == route.interface_id
                                    && service.kind == route.kind),
                            "invalid admitted route declaration: provider={} capability={} kind={:?} interface={} operation={}",
                            plugin.declaration.id.as_str(),
                            route.capability,
                            route.kind,
                            route.interface_id,
                            route.operation
                        );
                        server.approve_admitted_route(bmux_server::ServiceRoute {
                            capability: route.capability,
                            interface_id: route.interface_id,
                            operation: route.operation,
                            kind: match route.kind {
                                bmux_plugin_sdk::ServiceKind::Query => {
                                    bmux_ipc::InvokeServiceKind::Query
                                }
                                bmux_plugin_sdk::ServiceKind::Command => {
                                    bmux_ipc::InvokeServiceKind::Command
                                }
                                bmux_plugin_sdk::ServiceKind::Event => {
                                    anyhow::bail!("event cannot be an admitted request route")
                                }
                            },
                        })?;
                    }
                }
                if let bmux_plugin_sdk::PluginContribution::Extension {
                    extension_point,
                    payload,
                    ..
                } = contribution
                    && extension_point == STREAM_ADMISSION_PROVIDER_EXTENSION
                {
                    anyhow::ensure!(
                        selected.is_none(),
                        "multiple stream admission providers declared"
                    );
                    let declaration: StreamAdmissionProviderDeclaration =
                        bmux_plugin_sdk::decode_service_message(&payload)?;
                    let route = |operation: &bmux_plugin::stream_admission::StreamAdmissionProviderOperation, kind| {
                        anyhow::ensure!(plugin.declaration.services.iter().any(|service|
                            service.capability.as_str() == operation.capability
                            && service.interface_id == operation.interface_id
                            && service.kind == kind), "admission route not owned by declaring provider");
                        Ok::<_, anyhow::Error>(bmux_server::ServiceRoute {
                            capability: operation.capability.clone(),
                            kind: match kind {
                                bmux_plugin_sdk::ServiceKind::Query => bmux_ipc::InvokeServiceKind::Query,
                                bmux_plugin_sdk::ServiceKind::Command => bmux_ipc::InvokeServiceKind::Command,
                                bmux_plugin_sdk::ServiceKind::Event => anyhow::bail!("event cannot be an admission operation"),
                            },
                            interface_id: operation.interface_id.clone(), operation: operation.operation.clone(),
                        })
                    };
                    let routes = bmux_server::StreamAdmissionProviderRoutes {
                        probe: route(&declaration.probe, bmux_plugin_sdk::ServiceKind::Query)?,
                        consume: route(
                            &declaration.consume,
                            bmux_plugin_sdk::ServiceKind::Command,
                        )?,
                        validate: route(
                            &declaration.validate,
                            bmux_plugin_sdk::ServiceKind::Query,
                        )?,
                    };
                    selected = Some((routes, declaration));
                }
            }
        }
        if let Some((routes, declaration)) = selected {
            let deadline = std::time::Duration::from_millis(declaration.deadline_ms);
            let capacity = declaration.max_in_flight;
            server.install_stream_admission_provider(
                routes,
                std::sync::Arc::new(declaration),
                deadline,
                capacity,
            )?;
        }
    }
    // Spawn a plugin-bus → streaming-client forwarder task for every
    // `event_publications` entry that opted into
    // `forward_to_streaming_clients`. These live for the server's
    // lifetime; they terminate automatically when the server drops
    // (see `spawn_plugin_bus_forwarder` for the Weak-held shutdown
    // story).
    for plugin in loaded_plugins.iter() {
        for publication in &plugin.declaration.event_publications {
            if !publication.forward_to_streaming_clients {
                continue;
            }
            let kind = &publication.kind;
            let spawn_result = match (kind.as_str(), publication.delivery) {
                (k, bmux_plugin_sdk::PluginEventDelivery::State)
                    if k == bmux_scene_protocol::scene_protocol::STATE_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_state_forwarder::<
                        bmux_scene_protocol::scene_protocol::DecorationScene,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_contexts_plugin_api::contexts_events::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_contexts_plugin_api::contexts_events::ContextEvent,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_sessions_plugin_api::sessions_events::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_sessions_plugin_api::sessions_events::SessionEvent,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_clients_plugin_api::clients_events::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_clients_plugin_api::clients_events::ClientEvent,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_control_catalog_plugin_api::control_catalog_events::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_control_catalog_plugin_api::control_catalog_events::CatalogEvent,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_performance_plugin_api::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<bmux_performance_plugin_api::PerformanceEvent>(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_recording_plugin_api::recording_events::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_recording_plugin_api::recording_events::RecordingEvent,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::Broadcast)
                    if k == bmux_pane_runtime_plugin_api::pane_runtime_events::EVENT_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_forwarder::<
                        bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::State)
                    if k == bmux_tabs_plugin_api::tabs_list::STATE_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_state_forwarder::<
                        bmux_tabs_plugin_api::tabs_list::TabListSnapshot,
                    >(kind)
                }
                (k, bmux_plugin_sdk::PluginEventDelivery::State)
                    if k == bmux_appearance::RUNTIME_APPEARANCE_STATE_KIND.as_str() =>
                {
                    server.spawn_plugin_bus_state_forwarder::<bmux_appearance::RuntimeAppearance>(
                        kind,
                    )
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
    let server_identity = crate::connection::read_server_runtime_metadata()?
        .context("server metadata missing after publication")?;
    bmux_plugin::shutdown_progress()
        .configure(
            paths.runtime_dir.join("shutdown-progress.json"),
            server_identity.pid,
            server_identity.started_at_epoch_ms,
        )
        .map_err(anyhow::Error::msg)?;
    dispatch_loaded_plugin_event(&loaded_plugins, &plugin_system_event("server_started"))?;
    let gateway_loop = async move {
        match configured_gateway {
            Some(gateway) => gateway.run().await,
            None => std::future::pending::<Result<()>>().await,
        }
    };
    let mut gateway_loop = Box::pin(gateway_loop);
    let ownership = startup_ownership
        .as_ref()
        .context("foreground startup has no lifetime claim")?;
    let managed_idle_context = managed_idle_policy
        .as_ref()
        .map(|_| super::plugin_kernel::OwnedKernelContext::new(server.host_service_context()))
        .transpose()?;
    let managed_idle_caller = managed_idle_context
        .as_ref()
        .map(|context| {
            super::plugin_runtime::managed_companion_caller(
                &loaded_plugins,
                &config,
                &paths,
                context.bridge(),
            )
        })
        .transpose()?;
    let (server_ready_tx, server_ready_rx) = tokio::sync::oneshot::channel();
    let managed_idle = async {
        if !matches!(server_ready_rx.await, Ok(Ok(()))) {
            return std::future::pending::<()>().await;
        }
        let Some(caller) = managed_idle_caller.as_ref() else {
            return std::future::pending::<()>().await;
        };
        match super::managed_companion::watch(managed_idle_policy, Some(caller)).await {
            Ok(()) => server.request_shutdown(),
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "managed companion idle watcher disabled; shutdown not authorized");
            }
        }
        std::future::pending::<()>().await;
    };
    tokio::pin!(managed_idle);
    let mut server_loop = Box::pin(server.run_with_readiness(server_ready_tx));
    // The first poll performs synchronous bind. Capture its receipt before any
    // plugin restore await can turn a bound listener into a long startup window.
    let mut owned_server_loop = Box::pin(std::future::poll_fn(|cx| {
        use std::future::Future as _;
        let result = server_loop.as_mut().poll(cx);
        if !matches!(&result, std::task::Poll::Ready(Err(_)))
            && let Err(error) = ownership.record_bound_endpoint()
        {
            return std::task::Poll::Ready(Err(error));
        }
        result
    }));
    let run_result = if loaded_plugins.is_empty() {
        tokio::select! {
            biased;
            () = &mut managed_idle => unreachable!("idle watcher remains pending after shutdown request"),
            result = &mut owned_server_loop => result,
            result = &mut gateway_loop => result,
        }
    } else {
        let (plugin_bridge_shutdown_tx, plugin_bridge_shutdown_rx) =
            tokio::sync::watch::channel(false);
        let plugin_bridge = plugin_event_bridge_loop(
            std::sync::Arc::clone(&loaded_plugins),
            plugin_bridge_shutdown_rx,
        );
        tokio::pin!(plugin_bridge);
        tokio::select! {
            () = &mut managed_idle => unreachable!("idle watcher remains pending after shutdown request"),
            result = &mut owned_server_loop => {
                let _ = plugin_bridge_shutdown_tx.send(true);
                result
            }
            result = &mut plugin_bridge => {
                let _ = plugin_bridge_shutdown_tx.send(true);
                result?;
                Ok(())
            }
            result = &mut gateway_loop => {
                let _ = plugin_bridge_shutdown_tx.send(true);
                result
            }
        }
    };
    // Cancellation must release gateway listeners before plugin teardown, which
    // can take time. A pinned reference would keep the underlying future alive.
    drop(managed_idle_context);
    drop(owned_server_loop);
    drop(server_loop);
    drop(gateway_loop);
    if let Err(error) =
        dispatch_loaded_plugin_event(&loaded_plugins, &plugin_system_event("server_stopping"))
    {
        warn!("failed delivering server_stopping plugin event: {error}");
    }
    tracing::info!("shutdown plugin teardown started");
    let shutdown_result =
        Box::pin(deactivate_loaded_plugins(&loaded_plugins, &config, &paths)).await;
    let result = match (run_result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(run), Err(teardown)) => Err(anyhow::anyhow!(
            "shutdown failed: {run:#}; plugin teardown failed: {teardown:#}"
        )),
    };
    let receipt = serde_json::json!({
        "pid": server_identity.pid,
        "started_at_epoch_ms": server_identity.started_at_epoch_ms,
        "error": result.as_ref().err().map(|error| format!("{error:#}")),
    });
    let receipt_path = paths.runtime_dir.join("shutdown-result.json");
    let staged_receipt = paths
        .runtime_dir
        .join(format!("shutdown-result.{}.next", std::process::id()));
    std::fs::write(&staged_receipt, serde_json::to_vec(&receipt)?)?;
    std::fs::File::open(&staged_receipt)?.sync_all()?;
    std::fs::rename(&staged_receipt, &receipt_path)?;
    std::fs::File::open(&paths.runtime_dir)?.sync_all()?;
    // Do not erase ownership metadata published by a replacement instance.
    if crate::connection::read_server_runtime_metadata()?.as_ref() == Some(&server_identity) {
        remove_server_pid_file()?;
    }
    if let Err(error) = bmux_plugin::shutdown_progress()
        .advance("cleanup result published; waiting for process exit")
    {
        tracing::warn!(%error, "shutdown progress publication failed");
    }
    tracing::info!(error = ?result.as_ref().err(), "shutdown completed");
    result?;
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
    startup_recording: Option<ManualRecordingStartOptions>,
) {
    use bmux_recording_plugin_api::RecordingPluginConfig;
    let auto_export_dir = config.recording_auto_export_dir(paths);
    let plugin_config = RecordingPluginConfig {
        recordings_dir: config.recordings_dir(paths),
        rolling_recordings_dir: paths.rolling_recordings_dir(),
        rolling_segment_mb: config.recording.segment_mb,
        retention_days: config.recording.retention_days,
        rolling_defaults: rolling_settings.clone(),
        rolling_auto_start,
        auto_export: config.recording.auto_export || auto_export_dir.is_some(),
        auto_export_dir,
        auto_export_fps: config.recording.export.fps.max(1),
        startup_recording,
    };
    let handle = std::sync::Arc::new(std::sync::RwLock::new(plugin_config));
    bmux_plugin::global_plugin_state_registry().register::<RecordingPluginConfig>(&handle);
}

fn set_startup_recording_env(
    command: &mut ProcessCommand,
    options: &ManualRecordingStartOptions,
) -> Result<()> {
    let value =
        serde_json::to_string(options).context("failed to encode startup recording config")?;
    command.env(BOOTSTRAP_RECORDING_START_ENV, value);
    Ok(())
}

fn startup_recording_from_env() -> Result<Option<ManualRecordingStartOptions>> {
    let Ok(value) = std::env::var(BOOTSTRAP_RECORDING_START_ENV) else {
        return Ok(None);
    };
    serde_json::from_str(&value)
        .map(Some)
        .context("failed to decode startup recording config")
}

/// Register the snapshot plugin's startup config (file path +
/// debounce window) into the plugin state registry. Called between
/// `load_enabled_plugins` and `activate_loaded_plugins` so the
/// snapshot plugin's `activate` callback can read it and construct
/// its orchestrator. The file name is versioned (`bmux-snapshot-v1.json`)
/// so the new combined-envelope format never silently overwrites an
/// older monolithic snapshot. Managed targets keep this legacy locator visible
/// while the snapshot plugin coordinates exact-target entries in a shared durable
/// catalog beside it; they never restore another target's retained runtime.
fn register_snapshot_plugin_config(config: &BmuxConfig, paths: &ConfigPaths) {
    use bmux_snapshot_plugin_api::SnapshotPluginConfig;
    if !config.behavior.restore_last_layout {
        return;
    }
    let plugin_config = SnapshotPluginConfig {
        snapshot_path: paths.data_dir.join("runtime").join("bmux-snapshot-v1.json"),
        debounce_ms: 1_000,
    };
    let handle = std::sync::Arc::new(std::sync::RwLock::new(plugin_config));
    bmux_plugin::global_plugin_state_registry().register::<SnapshotPluginConfig>(&handle);
}

fn register_pane_runtime_plugin_config(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    pane_shell_integration_override: Option<bool>,
) {
    use bmux_pane_runtime_plugin_api::PaneRuntimePluginConfig;
    let plugin_config = PaneRuntimePluginConfig {
        shell: resolve_pane_runtime_shell(config),
        pane_term: resolve_pane_runtime_term(config),
        bracketed_paste: super::attach::runtime::bracketed_paste_enabled(
            config.behavior.bracketed_paste,
        ),
        shell_integration_root: pane_shell_integration_override
            .unwrap_or(config.behavior.pane_shell_integration)
            .then(|| paths.state_dir().join("runtime").join("shell-integration")),
    };
    let handle = std::sync::Arc::new(std::sync::RwLock::new(plugin_config));
    bmux_plugin::global_plugin_state_registry().register::<PaneRuntimePluginConfig>(&handle);
}

fn resolve_pane_runtime_shell(config: &BmuxConfig) -> String {
    if let Some(shell) = config.general.default_shell.as_ref()
        && !shell.trim().is_empty()
    {
        return shell.clone();
    }
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.trim().is_empty()
    {
        return shell;
    }
    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn resolve_pane_runtime_term(config: &BmuxConfig) -> String {
    const FALLBACKS: &[&str] = &["xterm-256color", "screen-256color"];
    let configured = config.behavior.pane_term.trim();
    let candidate = if configured.is_empty() {
        "xterm-256color"
    } else {
        configured
    };
    if check_terminfo_available(candidate) {
        return candidate.to_string();
    }
    for fallback in FALLBACKS {
        if *fallback != candidate && check_terminfo_available(fallback) {
            warn!(
                "pane TERM '{}' terminfo not installed; falling back to '{}'",
                candidate, fallback
            );
            return (*fallback).to_string();
        }
    }
    warn!(
        "pane TERM '{}' terminfo not installed and no fallback found; using as-is",
        candidate
    );
    candidate.to_string()
}

fn check_terminfo_available(term: &str) -> bool {
    std::process::Command::new("infocmp")
        .arg(term)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
    tracing::info!(
        target = target.unwrap_or(""),
        follow = follow.unwrap_or(""),
        global,
        "attach.bootstrap.explicit_start"
    );
    let provider = super::attach::provider::resolve(target)?;
    if !provider.requires_fallback_client() {
        let session = provider
            .open(
                None,
                bmux_client::AttachProviderOpenContext {
                    fallback_client: None,
                    endpoint_connector: Some(std::sync::Arc::new(CliAttachEndpointConnector)),
                },
            )
            .await?;
        return Box::pin(
            super::attach::runtime::run_session_attach_with_provider_session(
                session, target, follow, global, None,
            ),
        )
        .await
        .map(|outcome| outcome.status_code);
    }
    let client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-attach",
        connection_context,
    )
    .await?;
    tracing::info!("attach.bootstrap.explicit_connected");
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LogProcessScope {
    Server,
    Client,
    Command,
}

pub(super) fn init_logging(
    verbose: bool,
    cli_level: Option<LogLevel>,
    file_only: bool,
    scope: LogProcessScope,
) -> Option<RuntimeLoggingHandle> {
    let level = resolve_log_level(
        verbose,
        cli_level,
        std::env::var("BMUX_LOG_LEVEL").ok().as_deref(),
    );
    let tracing_level = tracing_level(level);
    let _ = EFFECTIVE_LOG_LEVEL.set(tracing_level);

    let paths = resolve_logging_paths();
    let runtime_level = match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    };
    let config = BmuxConfig::load().unwrap_or_default();
    let mode = log_mode_for_scope(&config, scope);
    if mode == DiagnosticLogMode::Segmented {
        match init_segmented_logging(&paths, &config, scope, runtime_level, file_only) {
            Ok(handle) => {
                if let RuntimeLoggingHandle::Diagnostic {
                    handle: control_handle,
                    ..
                } = &handle
                {
                    let _ = LOG_CONTROL.set(control_handle.control());
                }
                tracing::info!(
                    scope = ?scope,
                    mode = ?mode,
                    file_only,
                    pid = std::process::id(),
                    "logging.initialized"
                );
                return Some(handle);
            }
            Err(error) => {
                eprintln!("bmux warning: failed to initialize segmented logging: {error}");
            }
        }
        return None;
    }

    let mut log_config = moosicbox_log_runtime::init::InitConfig::new(&paths);
    log_config.default_env_filter = Some(format!("bmux={runtime_level}"));
    log_config.sinks.file = match mode {
        DiagnosticLogMode::Segmented => unreachable!("segmented mode handled above"),
        DiagnosticLogMode::Unified => Some(moosicbox_log_runtime::init::FileSinkConfig {
            mode: moosicbox_log_runtime::init::FileMode::Exact(unified_log_file_name(scope)),
        }),
        DiagnosticLogMode::Off => None,
    };
    // Commands that enter raw terminal mode (attach, connect, join, etc.) must not
    // write tracing output to stderr — it would corrupt the TUI. Non-raw-mode
    // commands keep stderr for interactive debugging.
    if file_only {
        log_config.sinks.stderr = false;
    }
    match moosicbox_log_runtime::init::init(log_config) {
        Ok(handle) => {
            tracing::info!(
                scope = ?scope,
                mode = ?mode,
                file_only,
                pid = std::process::id(),
                "logging.initialized"
            );
            return Some(RuntimeLoggingHandle::Moosicbox { _handle: handle });
        }
        Err(error) => {
            eprintln!("bmux warning: failed to initialize file logging: {error}");
        }
    }
    None
}

const fn log_mode_for_scope(config: &BmuxConfig, scope: LogProcessScope) -> DiagnosticLogMode {
    match scope {
        LogProcessScope::Server => config_log_mode(config.logs.server.mode),
        LogProcessScope::Client => config_log_mode(config.logs.client.mode),
        LogProcessScope::Command => DiagnosticLogMode::Unified,
    }
}

const fn config_log_mode(mode: bmux_config::DiagnosticLogModeConfig) -> DiagnosticLogMode {
    match mode {
        bmux_config::DiagnosticLogModeConfig::Segmented => DiagnosticLogMode::Segmented,
        bmux_config::DiagnosticLogModeConfig::Unified => DiagnosticLogMode::Unified,
        bmux_config::DiagnosticLogModeConfig::Off => DiagnosticLogMode::Off,
    }
}

const fn unified_log_file_name(scope: LogProcessScope) -> &'static str {
    match scope {
        LogProcessScope::Server | LogProcessScope::Command => "bmux.log",
        LogProcessScope::Client => "bmux-client.log",
    }
}

fn init_segmented_logging(
    paths: &moosicbox_log_runtime::LogRuntimePaths,
    config: &BmuxConfig,
    scope: LogProcessScope,
    runtime_level: &str,
    file_only: bool,
) -> anyhow::Result<RuntimeLoggingHandle> {
    use tracing_subscriber::layer::SubscriberExt as _;

    let (root_dir, file_prefix, segment_mb, retention_days, max_total_mb) = match scope {
        LogProcessScope::Server | LogProcessScope::Command => (
            paths.log_dir.join("server"),
            "server",
            config.logs.server.segment_mb,
            config.logs.server.retention_days,
            config.logs.server.max_total_mb,
        ),
        LogProcessScope::Client => (
            paths.log_dir.join("clients"),
            "client",
            config.logs.client.segment_mb,
            config.logs.client.retention_days,
            config.logs.client.max_total_mb,
        ),
    };
    let run_id = format!("{}-{}", std::process::id(), Uuid::new_v4());
    let (writer, handle) = DiagnosticLogWriter::start(DiagnosticLogConfig {
        root_dir,
        kind: file_prefix.to_owned(),
        file_prefix: file_prefix.to_owned(),
        run_id,
        mode: DiagnosticLogMode::Segmented,
        segment_mb,
        retention_days,
        max_total_mb,
        subject_id: None,
    })?;
    let (non_blocking_writer, guard) = tracing_appender::non_blocking(writer);
    let env_filter = tracing_subscriber::EnvFilter::try_new(format!("bmux={runtime_level}"))?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(non_blocking_writer);
    let stderr_layer =
        (!file_only).then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
    let diagnostics_layer =
        OperationalDiagnosticsLayer::new(config.diagnostics.clone(), &paths.state_dir);
    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .with(diagnostics_layer);
    tracing::subscriber::set_global_default(subscriber)?;
    let _ = tracing_log::LogTracer::init();
    Ok(RuntimeLoggingHandle::Diagnostic {
        _guard: guard,
        handle,
    })
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
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::runtime::session_cli::attach_quit_failure_status;
    use bmux_client::ClientError;
    use bmux_ipc::ErrorCode;
    use bmux_ipc::transport::IpcTransportError;

    #[tokio::test]
    async fn startup_log_creates_missing_runtime_directory_and_truncates_on_restart() {
        use std::io::Write;

        let root = tempfile::tempdir().expect("temporary directory");
        let runtime_dir = root.path().join("runtimes").join("test");
        assert!(!runtime_dir.exists());
        let mut log = open_server_startup_log(&runtime_dir).expect("open startup log");
        log.write_all(b"startup diagnostics")
            .expect("write diagnostics");
        drop(log);
        let path = runtime_dir.join("server-startup.log");
        assert_eq!(
            std::fs::read(&path).expect("read log"),
            b"startup diagnostics"
        );
        assert_eq!(startup_diagnostics_tail(&path), "startup diagnostics");
        assert!(!root.path().join("server-startup.log").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&runtime_dir)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            let endpoint = bmux_ipc::IpcEndpoint::unix_socket(runtime_dir.join("test.sock"));
            let listener = bmux_ipc::transport::LocalIpcListener::bind(&endpoint)
                .expect("startup-created directory must support secure socket binding");
            drop(listener);
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("log metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(open_server_startup_log(&runtime_dir).expect("reopen startup log"));
        assert_eq!(
            std::fs::read(&path).expect("read truncated log"),
            [] as [u8; 0]
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_log_allows_shared_ancestors_but_rejects_symlink_artifacts() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let runtime = root.path().join("runtime");
        drop(open_server_startup_log(&runtime).unwrap());
        let log = runtime.join("server-startup.log");
        std::fs::remove_file(&log).unwrap();
        let target = root.path().join("untouched");
        std::fs::write(&target, b"unchanged").unwrap();
        symlink(&target, &log).unwrap();
        assert!(open_server_startup_log(&runtime).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn startup_log_reports_invalid_runtime_directory() {
        let root = tempfile::tempdir().expect("temporary directory");
        let runtime_dir = root.path().join("not-a-directory");
        std::fs::write(&runtime_dir, b"existing file").expect("create blocking file");
        let error = open_server_startup_log(&runtime_dir).expect_err("must reject file");
        assert!(
            error
                .to_string()
                .contains("failed creating server runtime directory")
        );
        assert!(
            error
                .to_string()
                .contains(&runtime_dir.display().to_string())
        );
        assert_eq!(
            std::fs::read(runtime_dir).expect("read existing file"),
            b"existing file"
        );
    }

    #[test]
    fn manual_daemon_child_arguments_preserve_foreground_internal_contract() {
        let arguments =
            server_start_child_arguments(None, &RecordingRollingStartOptions::default(), None);
        let runtime = active_runtime_name();
        assert_eq!(
            arguments,
            [
                "--runtime",
                runtime.as_str(),
                "server",
                "start",
                "--foreground-internal",
            ]
        );
        assert!(!arguments.iter().any(|argument| argument == "--daemon"));
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
}
