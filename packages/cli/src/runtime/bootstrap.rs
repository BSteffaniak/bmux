use super::*;

#[derive(Debug, Clone)]
pub(super) struct DefaultAttachOptions {
    pub(super) record: bool,
    pub(super) capture_input: bool,
    pub(super) profile: Option<RecordingProfileArg>,
    pub(super) event_kinds: Vec<RecordingEventKindArg>,
    pub(super) recording_id_file: Option<String>,
    pub(super) stop_server_on_exit: bool,
}

#[derive(Debug, Clone)]
pub(super) struct AttachDisplayCapturePlan {
    pub(super) recording_id: Uuid,
    pub(super) recording_path: PathBuf,
}

pub(super) async fn run_default_server_attach(options: DefaultAttachOptions) -> Result<u8> {
    if options.record {
        ensure_server_not_running_for_record_bootstrap().await?;
    }
    ensure_server_running_for_default_attach().await?;

    let mut active_recording_id = None;
    let mut capture_plan = None;
    if options.record {
        let mut recording_client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-start",
        )
        .await?;
        let started = recording_client
            .recording_start(
                None,
                options.capture_input,
                recording::recording_profile_arg_to_ipc(options.profile),
                recording::resolve_event_kind_override(
                    options.profile,
                    &options.event_kinds,
                    options.capture_input,
                ),
            )
            .await
            .map_err(map_cli_client_error)?;
        active_recording_id = Some(started.id);
        capture_plan = Some(AttachDisplayCapturePlan {
            recording_id: started.id,
            recording_path: PathBuf::from(&started.path),
        });
        println!(
            "recording started: {} (capture_input={})",
            started.id, started.capture_input
        );
        if let Some(path) = options.recording_id_file.as_deref() {
            std::fs::write(path, format!("{}\n", started.id))
                .with_context(|| format!("failed writing recording id file {path}"))?;
        }
    }

    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-default-attach").await?;
    let target = resolve_default_attach_target(&mut client).await?;
    let target = target.to_string();
    let attach_result =
        run_session_attach_with_client(client, Some(target.as_str()), None, false, capture_plan)
            .await;

    if let Some(recording_id) = active_recording_id {
        let mut stop_client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-stop",
        )
        .await?;
        let stopped_id = stop_client
            .recording_stop(Some(recording_id))
            .await
            .map_err(map_cli_client_error)
            .with_context(|| format!("failed stopping recording {recording_id}"))?;
        let mut list_client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-default-attach-recording-list",
        )
        .await?;
        let recording = list_client
            .recording_list()
            .await
            .map_err(map_cli_client_error)?
            .into_iter()
            .find(|summary| summary.id == stopped_id);
        if let Some(recording) = recording {
            println!(
                "recording stopped: {} events={} bytes={} path={}",
                recording.id, recording.event_count, recording.payload_bytes, recording.path
            );
        } else {
            println!("recording stopped: {stopped_id}");
        }
    }

    if options.record && options.stop_server_on_exit {
        let _ = run_server_stop().await;
    }

    attach_result
}

pub(super) async fn ensure_server_not_running_for_record_bootstrap() -> Result<()> {
    if server_is_running().await? {
        anyhow::bail!(
            "--record requires a fresh start but server is already running; stop it first or run without --record"
        )
    }
    Ok(())
}

pub(super) async fn ensure_server_running_for_default_attach() -> Result<()> {
    if server_is_running().await? {
        return Ok(());
    }

    let _ = run_server_start(true, false).await?;
    if !server_is_running().await? {
        anyhow::bail!("bmux server failed to start for default attach")
    }
    Ok(())
}

pub(super) async fn resolve_default_attach_target(client: &mut BmuxClient) -> Result<Uuid> {
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if sessions.is_empty() {
        let name = next_default_session_name(&sessions);
        let id = client
            .new_session(Some(name.clone()))
            .await
            .map_err(map_cli_client_error)?;
        return Ok(id);
    }

    let _client_id = client.whoami().await.map_err(map_cli_client_error)?;
    let writable_sessions = sessions.clone();

    if writable_sessions.is_empty() {
        let name = next_default_session_name(&sessions);
        let id = client
            .new_session(Some(name.clone()))
            .await
            .map_err(map_cli_client_error)?;
        return Ok(id);
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

pub(super) fn next_default_session_name(sessions: &[SessionSummary]) -> String {
    let mut next = 1_u32;
    loop {
        let candidate = format!("session-{next}");
        if sessions
            .iter()
            .all(|session| session.name.as_deref() != Some(candidate.as_str()))
        {
            return candidate;
        }
        next = next.saturating_add(1);
    }
}

pub(super) async fn run_server_start(daemon: bool, foreground_internal: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    if server_is_running().await? {
        println!("bmux server is already running");
        return Ok(1);
    }

    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    validate_enabled_plugins(&config, &registry)?;
    let _preloaded_plugins = load_enabled_plugins(&config, &registry)?;

    if daemon && !foreground_internal {
        let executable =
            std::env::current_exe().context("failed to resolve bmux executable path")?;
        let mut child = ProcessCommand::new(executable);
        let log_level = EFFECTIVE_LOG_LEVEL.get().copied().unwrap_or(Level::INFO);
        child
            .arg("server")
            .arg("start")
            .arg("--foreground-internal")
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

        if !wait_for_server_running(SERVER_START_TIMEOUT).await? {
            let _ = try_kill_pid(child.id());
            let _ = remove_server_pid_file();
            anyhow::bail!("background server did not become ready before timeout")
        }

        println!("bmux server started in daemon mode (pid {})", child.id());
        return Ok(0);
    }

    let loaded_plugins = load_enabled_plugins(&config, &registry)?;
    activate_loaded_plugins(&loaded_plugins, &config, &paths)?;
    dispatch_loaded_plugin_event(&loaded_plugins, plugin_system_event("server_starting"))?;
    let server = BmuxServer::from_config_paths(&paths);
    register_plugin_service_handlers(&server, &config, &paths, &registry)?;
    write_server_pid_file(std::process::id())?;
    write_server_runtime_metadata(std::process::id())?;
    dispatch_loaded_plugin_event(&loaded_plugins, plugin_system_event("server_started"))?;
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
        dispatch_loaded_plugin_event(&loaded_plugins, plugin_system_event("server_stopping"))
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

pub(super) async fn run_session_attach(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    let client = connect(ConnectionPolicyScope::Normal, "bmux-cli-attach").await?;
    run_session_attach_with_client(client, target, follow, global, None).await
}

pub(super) fn map_attach_client_error(error: ClientError) -> anyhow::Error {
    match error {
        ClientError::ServerError { code, message } => match code {
            bmux_ipc::ErrorCode::AlreadyExists => {
                anyhow::anyhow!("attach failed: session already has an active attached client")
            }
            bmux_ipc::ErrorCode::NotFound => anyhow::anyhow!("attach failed: {message}"),
            _ => anyhow::anyhow!("attach failed: {message}"),
        },
        other => map_client_connect_error(other),
    }
}

pub(super) fn map_cli_client_error(error: ClientError) -> anyhow::Error {
    map_client_connect_error(error)
}

pub(super) fn init_logging(verbose: bool, cli_level: Option<LogLevel>) {
    let level = resolve_log_level(
        verbose,
        cli_level,
        std::env::var("BMUX_LOG_LEVEL").ok().as_deref(),
    );
    let tracing_level = tracing_level(level);
    let _ = EFFECTIVE_LOG_LEVEL.set(tracing_level);

    {
        let paths =
            moosicbox_log_runtime::resolve_paths(&moosicbox_log_runtime::LogRuntimePathsConfig {
                app_name: "bmux",
                state_dir_env: "BMUX_STATE_DIR",
                log_dir_env: "BMUX_LOG_DIR",
            });
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
#[cfg(test)]
mod tests {
    fn empty_cli() -> bmux_cli_schema::Cli {
        bmux_cli_schema::Cli {
            record: false,
            no_capture_input: false,
            recording_id_file: None,
            record_profile: None,
            record_event_kind: Vec::new(),
            stop_server_on_exit: false,
            target: None,
            command: None,
            verbose: false,
            log_level: None,
        }
    }

    use crate::runtime::*;
    use bmux_cli_schema::Command;
    use bmux_client::ClientError;
    use bmux_config::BmuxConfig;
    use bmux_ipc::ErrorCode;
    use bmux_ipc::transport::IpcTransportError;

    #[test]
    fn validate_record_bootstrap_flags_accepts_plain_defaults() {
        let cli = empty_cli();
        assert!(crate::runtime::validate_record_bootstrap_flags(&cli).is_ok());
    }

    #[test]
    fn validate_record_bootstrap_flags_rejects_orphaned_record_flags() {
        let mut cli = empty_cli();
        cli.no_capture_input = true;
        let error = crate::runtime::validate_record_bootstrap_flags(&cli)
            .expect_err("validation should fail");
        assert!(
            error
                .to_string()
                .contains("--no-capture-input requires --record"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn validate_record_bootstrap_flags_rejects_record_with_subcommand() {
        let mut cli = empty_cli();
        cli.record = true;
        cli.command = Some(Command::ListSessions { json: false });
        let error = crate::runtime::validate_record_bootstrap_flags(&cli)
            .expect_err("validation should fail");
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
        let status = crate::runtime::attach_quit_failure_status(&ClientError::ServerError {
            code: ErrorCode::InvalidRequest,
            message: "session policy denied for this operation".to_string(),
        });

        assert_eq!(status, "quit blocked by session policy");
    }

    #[test]
    fn initial_attach_status_mentions_help_and_typing() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let status = crate::runtime::initial_attach_status(&keymap, true);
        assert!(status.contains("help"));
        assert!(status.contains("typing goes to pane"));
    }
}
