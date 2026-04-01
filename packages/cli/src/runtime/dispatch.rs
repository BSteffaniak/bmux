use super::*;

pub(super) async fn run_command(command: &Command) -> Result<u8> {
    match command {
        Command::External(args) => run_external_plugin_command(args).await,
        _ => dispatch_built_in_command(command).await,
    }
}

pub(super) fn built_in_handler_for_command(command: &Command) -> BuiltInHandlerId {
    match command {
        Command::Connect { .. } => BuiltInHandlerId::Connect,
        Command::NewSession { .. } => BuiltInHandlerId::NewSession,
        Command::ListSessions { .. } => BuiltInHandlerId::ListSessions,
        Command::ListClients { .. } => BuiltInHandlerId::ListClients,
        Command::KillSession { .. } => BuiltInHandlerId::KillSession,
        Command::KillAllSessions { .. } => BuiltInHandlerId::KillAllSessions,
        Command::Attach { .. } => BuiltInHandlerId::Attach,
        Command::Detach => BuiltInHandlerId::Detach,
        Command::Follow { .. } => BuiltInHandlerId::Follow,
        Command::Unfollow => BuiltInHandlerId::Unfollow,
        Command::Session { command } => match command {
            SessionCommand::New { .. } => BuiltInHandlerId::SessionNew,
            SessionCommand::List { .. } => BuiltInHandlerId::SessionList,
            SessionCommand::Clients { .. } => BuiltInHandlerId::SessionClients,
            SessionCommand::Kill { .. } => BuiltInHandlerId::SessionKill,
            SessionCommand::KillAll { .. } => BuiltInHandlerId::SessionKillAll,
            SessionCommand::Attach { .. } => BuiltInHandlerId::SessionAttach,
            SessionCommand::Detach => BuiltInHandlerId::SessionDetach,
            SessionCommand::Follow { .. } => BuiltInHandlerId::SessionFollow,
            SessionCommand::Unfollow => BuiltInHandlerId::SessionUnfollow,
        },
        Command::Remote { command } => match command {
            RemoteCommand::List { .. } => BuiltInHandlerId::RemoteList,
            RemoteCommand::Test { .. } => BuiltInHandlerId::RemoteTest,
            RemoteCommand::Doctor { .. } => BuiltInHandlerId::RemoteDoctor,
            RemoteCommand::Init { .. } => BuiltInHandlerId::RemoteInit,
            RemoteCommand::InstallServer { .. } => BuiltInHandlerId::RemoteInstallServer,
            RemoteCommand::Upgrade { .. } => BuiltInHandlerId::RemoteUpgrade,
            RemoteCommand::Complete { command } => match command {
                RemoteCompleteCommand::Targets => BuiltInHandlerId::RemoteCompleteTargets,
                RemoteCompleteCommand::Sessions { .. } => BuiltInHandlerId::RemoteCompleteSessions,
            },
        },
        Command::Server { command } => match command {
            ServerCommand::Start { .. } => BuiltInHandlerId::ServerStart,
            ServerCommand::Status { .. } => BuiltInHandlerId::ServerStatus,
            ServerCommand::WhoamiPrincipal { .. } => BuiltInHandlerId::ServerWhoamiPrincipal,
            ServerCommand::Save => BuiltInHandlerId::ServerSave,
            ServerCommand::Restore { .. } => BuiltInHandlerId::ServerRestore,
            ServerCommand::Stop => BuiltInHandlerId::ServerStop,
            ServerCommand::Gateway { .. } => BuiltInHandlerId::ServerGateway,
            ServerCommand::Bridge { .. } => BuiltInHandlerId::ServerBridge,
        },
        Command::Logs { command } => match command {
            LogsCommand::Path { .. } => BuiltInHandlerId::LogsPath,
            LogsCommand::Level { .. } => BuiltInHandlerId::LogsLevel,
            LogsCommand::Tail { .. } => BuiltInHandlerId::LogsTail,
            LogsCommand::Watch { .. } => BuiltInHandlerId::LogsWatch,
            LogsCommand::Profiles { command } => match command {
                LogsProfilesCommand::List { .. } => BuiltInHandlerId::LogsProfilesList,
                LogsProfilesCommand::Show { .. } => BuiltInHandlerId::LogsProfilesShow,
                LogsProfilesCommand::Delete { .. } => BuiltInHandlerId::LogsProfilesDelete,
                LogsProfilesCommand::Rename { .. } => BuiltInHandlerId::LogsProfilesRename,
            },
        },
        Command::Config { command } => match command {
            ConfigCommand::Path { .. } => BuiltInHandlerId::ConfigPath,
            ConfigCommand::Show { .. } => BuiltInHandlerId::ConfigShow,
            ConfigCommand::Get { .. } => BuiltInHandlerId::ConfigGet,
            ConfigCommand::Set { .. } => BuiltInHandlerId::ConfigSet,
        },
        Command::Doctor { .. } => BuiltInHandlerId::Doctor,
        Command::Keymap { .. } => BuiltInHandlerId::KeymapDoctor,
        Command::Terminal { command } => match command {
            TerminalCommand::Doctor { .. } => BuiltInHandlerId::TerminalDoctor,
            TerminalCommand::InstallTerminfo { .. } => BuiltInHandlerId::TerminalInstallTerminfo,
        },
        Command::Recording { command } => match command {
            RecordingCommand::Start { .. } => BuiltInHandlerId::RecordingStart,
            RecordingCommand::Stop { .. } => BuiltInHandlerId::RecordingStop,
            RecordingCommand::Status { .. } => BuiltInHandlerId::RecordingStatus,
            RecordingCommand::List { .. } => BuiltInHandlerId::RecordingList,
            RecordingCommand::Delete { .. } => BuiltInHandlerId::RecordingDelete,
            RecordingCommand::DeleteAll { .. } => BuiltInHandlerId::RecordingDeleteAll,
            RecordingCommand::Inspect { .. } => BuiltInHandlerId::RecordingInspect,
            RecordingCommand::Replay { .. } => BuiltInHandlerId::RecordingReplay,
            RecordingCommand::VerifySmoke { .. } => BuiltInHandlerId::RecordingVerifySmoke,
            RecordingCommand::Export { .. } => BuiltInHandlerId::RecordingExport,
            RecordingCommand::Prune { .. } => BuiltInHandlerId::RecordingPrune,
        },
        Command::Playbook { command } => match command {
            PlaybookCommand::Run { .. } => BuiltInHandlerId::PlaybookRun,
            PlaybookCommand::Validate { .. } => BuiltInHandlerId::PlaybookValidate,
            PlaybookCommand::Interactive { .. } => BuiltInHandlerId::PlaybookInteractive,
            PlaybookCommand::FromRecording { .. } => BuiltInHandlerId::PlaybookFromRecording,
            PlaybookCommand::DryRun { .. } => BuiltInHandlerId::PlaybookDryRun,
            PlaybookCommand::Diff { .. } => BuiltInHandlerId::PlaybookDiff,
            PlaybookCommand::Cleanup { .. } => BuiltInHandlerId::PlaybookCleanup,
        },
        Command::External(_) => unreachable!("external commands are dispatched separately"),
    }
}

pub(super) async fn dispatch_built_in_command(command: &Command) -> Result<u8> {
    let handler = built_in_handler_for_command(command);
    let _descriptor = built_in_command_by_handler(handler);
    match (handler, command) {
        (
            BuiltInHandlerId::Connect,
            Command::Connect {
                target,
                session,
                follow,
                global,
                reconnect_forever,
            },
        ) => {
            run_connect(
                target,
                session.as_deref(),
                follow.as_deref(),
                *global,
                *reconnect_forever,
            )
            .await
        }
        (BuiltInHandlerId::NewSession, Command::NewSession { name }) => {
            run_session_new(name.clone()).await
        }
        (BuiltInHandlerId::ListSessions, Command::ListSessions { json }) => {
            run_session_list(*json).await
        }
        (BuiltInHandlerId::ListClients, Command::ListClients { json }) => {
            run_client_list(*json).await
        }
        (
            BuiltInHandlerId::KillSession,
            Command::KillSession {
                target,
                force_local,
            },
        ) => run_session_kill(target, *force_local).await,
        (BuiltInHandlerId::KillAllSessions, Command::KillAllSessions { force_local }) => {
            run_session_kill_all(*force_local).await
        }
        (
            BuiltInHandlerId::Attach,
            Command::Attach {
                target,
                follow,
                global,
            },
        ) => run_session_attach(target.as_deref(), follow.as_deref(), *global).await,
        (BuiltInHandlerId::Detach, Command::Detach) => run_session_detach().await,
        (
            BuiltInHandlerId::Follow,
            Command::Follow {
                target_client_id,
                global,
            },
        ) => run_follow(target_client_id, *global).await,
        (BuiltInHandlerId::Unfollow, Command::Unfollow) => run_unfollow().await,
        (
            BuiltInHandlerId::SessionNew,
            Command::Session {
                command: SessionCommand::New { name },
            },
        ) => run_session_new(name.clone()).await,
        (
            BuiltInHandlerId::SessionList,
            Command::Session {
                command: SessionCommand::List { json },
            },
        ) => run_session_list(*json).await,
        (
            BuiltInHandlerId::SessionClients,
            Command::Session {
                command: SessionCommand::Clients { json },
            },
        ) => run_client_list(*json).await,
        (
            BuiltInHandlerId::SessionKill,
            Command::Session {
                command:
                    SessionCommand::Kill {
                        target,
                        force_local,
                    },
            },
        ) => run_session_kill(target, *force_local).await,
        (
            BuiltInHandlerId::SessionKillAll,
            Command::Session {
                command: SessionCommand::KillAll { force_local },
            },
        ) => run_session_kill_all(*force_local).await,
        (
            BuiltInHandlerId::SessionAttach,
            Command::Session {
                command:
                    SessionCommand::Attach {
                        target,
                        follow,
                        global,
                    },
            },
        ) => run_session_attach(target.as_deref(), follow.as_deref(), *global).await,
        (
            BuiltInHandlerId::SessionDetach,
            Command::Session {
                command: SessionCommand::Detach,
            },
        ) => run_session_detach().await,
        (
            BuiltInHandlerId::SessionFollow,
            Command::Session {
                command:
                    SessionCommand::Follow {
                        target_client_id,
                        global,
                    },
            },
        ) => run_follow(target_client_id, *global).await,
        (
            BuiltInHandlerId::SessionUnfollow,
            Command::Session {
                command: SessionCommand::Unfollow,
            },
        ) => run_unfollow().await,
        (
            BuiltInHandlerId::RemoteList,
            Command::Remote {
                command: RemoteCommand::List { json },
            },
        ) => run_remote_list(*json),
        (
            BuiltInHandlerId::RemoteTest,
            Command::Remote {
                command: RemoteCommand::Test { target },
            },
        ) => run_remote_test(target).await,
        (
            BuiltInHandlerId::RemoteDoctor,
            Command::Remote {
                command: RemoteCommand::Doctor { target, fix },
            },
        ) => run_remote_doctor(target, *fix).await,
        (
            BuiltInHandlerId::RemoteInit,
            Command::Remote {
                command:
                    RemoteCommand::Init {
                        name,
                        ssh,
                        tls,
                        user,
                        port,
                        set_default,
                    },
            },
        ) => {
            run_remote_init(
                name,
                ssh.as_deref(),
                tls.as_deref(),
                user.as_deref(),
                *port,
                *set_default,
            )
            .await
        }
        (
            BuiltInHandlerId::RemoteInstallServer,
            Command::Remote {
                command: RemoteCommand::InstallServer { target },
            },
        ) => run_remote_install_server(target).await,
        (
            BuiltInHandlerId::RemoteUpgrade,
            Command::Remote {
                command: RemoteCommand::Upgrade { target },
            },
        ) => run_remote_upgrade(target.as_deref()).await,
        (
            BuiltInHandlerId::RemoteCompleteTargets,
            Command::Remote {
                command:
                    RemoteCommand::Complete {
                        command: RemoteCompleteCommand::Targets,
                    },
            },
        ) => run_remote_complete_targets(),
        (
            BuiltInHandlerId::RemoteCompleteSessions,
            Command::Remote {
                command:
                    RemoteCommand::Complete {
                        command: RemoteCompleteCommand::Sessions { target },
                    },
            },
        ) => run_remote_complete_sessions(target).await,
        (
            BuiltInHandlerId::ServerStart,
            Command::Server {
                command:
                    ServerCommand::Start {
                        daemon,
                        foreground_internal,
                    },
            },
        ) => run_server_start(*daemon, *foreground_internal).await,
        (
            BuiltInHandlerId::ServerStatus,
            Command::Server {
                command: ServerCommand::Status { json },
            },
        ) => run_server_status(*json).await,
        (
            BuiltInHandlerId::ServerWhoamiPrincipal,
            Command::Server {
                command: ServerCommand::WhoamiPrincipal { json },
            },
        ) => run_server_whoami_principal(*json).await,
        (
            BuiltInHandlerId::ServerSave,
            Command::Server {
                command: ServerCommand::Save,
            },
        ) => run_server_save().await,
        (
            BuiltInHandlerId::ServerRestore,
            Command::Server {
                command: ServerCommand::Restore { dry_run, yes },
            },
        ) => run_server_restore(*dry_run, *yes).await,
        (
            BuiltInHandlerId::ServerStop,
            Command::Server {
                command: ServerCommand::Stop,
            },
        ) => run_server_stop().await,
        (
            BuiltInHandlerId::ServerGateway,
            Command::Server {
                command:
                    ServerCommand::Gateway {
                        listen,
                        quick,
                        cert_file,
                        key_file,
                    },
            },
        ) => run_server_gateway(listen, *quick, cert_file.as_deref(), key_file.as_deref()).await,
        (
            BuiltInHandlerId::ServerBridge,
            Command::Server {
                command: ServerCommand::Bridge { stdio, preflight },
            },
        ) => run_server_bridge(*stdio, *preflight).await,
        (
            BuiltInHandlerId::LogsPath,
            Command::Logs {
                command: LogsCommand::Path { json },
            },
        ) => run_logs_path(*json),
        (
            BuiltInHandlerId::LogsLevel,
            Command::Logs {
                command: LogsCommand::Level { json },
            },
        ) => run_logs_level(*json),
        (
            BuiltInHandlerId::LogsTail,
            Command::Logs {
                command:
                    LogsCommand::Tail {
                        lines,
                        since,
                        no_follow,
                    },
            },
        ) => run_logs_tail(*lines, since.as_deref(), !*no_follow),
        (
            BuiltInHandlerId::LogsWatch,
            Command::Logs {
                command:
                    LogsCommand::Watch {
                        lines,
                        since,
                        profile,
                        include,
                        include_i,
                        exclude,
                        exclude_i,
                    },
            },
        ) => run_logs_watch(
            *lines,
            since.as_deref(),
            profile.as_deref(),
            include,
            include_i,
            exclude,
            exclude_i,
        ),
        (
            BuiltInHandlerId::LogsProfilesList,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::List { json },
                    },
            },
        ) => run_logs_profiles_list(*json),
        (
            BuiltInHandlerId::LogsProfilesShow,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Show { profile, json },
                    },
            },
        ) => run_logs_profiles_show(profile.as_deref(), *json),
        (
            BuiltInHandlerId::LogsProfilesDelete,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Delete { profile },
                    },
            },
        ) => run_logs_profiles_delete(profile),
        (
            BuiltInHandlerId::LogsProfilesRename,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Rename { from, to },
                    },
            },
        ) => run_logs_profiles_rename(from, to),
        (
            BuiltInHandlerId::ConfigPath,
            Command::Config {
                command: ConfigCommand::Path { json },
            },
        ) => run_config_path(*json),
        (
            BuiltInHandlerId::ConfigShow,
            Command::Config {
                command: ConfigCommand::Show { json },
            },
        ) => run_config_show(*json),
        (
            BuiltInHandlerId::ConfigGet,
            Command::Config {
                command: ConfigCommand::Get { key, json },
            },
        ) => run_config_get(key, *json),
        (
            BuiltInHandlerId::ConfigSet,
            Command::Config {
                command: ConfigCommand::Set { key, value },
            },
        ) => run_config_set(key, value),
        (BuiltInHandlerId::Doctor, Command::Doctor { json }) => run_doctor(*json).await,
        (
            BuiltInHandlerId::KeymapDoctor,
            Command::Keymap {
                command: KeymapCommand::Doctor { json },
            },
        ) => run_keymap_doctor(*json),
        (
            BuiltInHandlerId::TerminalDoctor,
            Command::Terminal {
                command:
                    TerminalCommand::Doctor {
                        json,
                        trace,
                        trace_limit,
                        trace_family,
                        trace_pane,
                    },
            },
        ) => run_terminal_doctor(*json, *trace, *trace_limit, *trace_family, *trace_pane),
        (
            BuiltInHandlerId::TerminalInstallTerminfo,
            Command::Terminal {
                command: TerminalCommand::InstallTerminfo { yes, check },
            },
        ) => run_terminal_install_terminfo(*yes, *check),
        (
            BuiltInHandlerId::RecordingStart,
            Command::Recording {
                command:
                    RecordingCommand::Start {
                        session_id,
                        no_capture_input,
                        profile,
                        event_kind,
                    },
            },
        ) => {
            run_recording_start(
                session_id.as_deref(),
                !*no_capture_input,
                *profile,
                event_kind,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingStop,
            Command::Recording {
                command: RecordingCommand::Stop { recording_id },
            },
        ) => run_recording_stop(recording_id.as_deref()).await,
        (
            BuiltInHandlerId::RecordingStatus,
            Command::Recording {
                command: RecordingCommand::Status { json },
            },
        ) => run_recording_status(*json).await,
        (
            BuiltInHandlerId::RecordingList,
            Command::Recording {
                command: RecordingCommand::List { json },
            },
        ) => run_recording_list(*json).await,
        (
            BuiltInHandlerId::RecordingDelete,
            Command::Recording {
                command: RecordingCommand::Delete { recording_id },
            },
        ) => run_recording_delete(recording_id).await,
        (
            BuiltInHandlerId::RecordingDeleteAll,
            Command::Recording {
                command: RecordingCommand::DeleteAll { yes },
            },
        ) => run_recording_delete_all(*yes).await,
        (
            BuiltInHandlerId::RecordingInspect,
            Command::Recording {
                command:
                    RecordingCommand::Inspect {
                        recording_id,
                        limit,
                        kind,
                        json,
                    },
            },
        ) => run_recording_inspect(recording_id, *limit, kind.as_deref(), *json),
        (
            BuiltInHandlerId::RecordingReplay,
            Command::Recording {
                command:
                    RecordingCommand::Replay {
                        recording_id,
                        mode,
                        speed,
                        target_bmux,
                        compare_recording,
                        ignore,
                        strict_timing,
                        max_verify_duration,
                        verify_start_timeout,
                    },
            },
        ) => {
            run_recording_replay(
                recording_id,
                *mode,
                *speed,
                target_bmux.as_deref(),
                compare_recording.as_deref(),
                ignore.as_deref(),
                *strict_timing,
                *max_verify_duration,
                *verify_start_timeout,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingVerifySmoke,
            Command::Recording {
                command:
                    RecordingCommand::VerifySmoke {
                        recording_id,
                        target_bmux,
                        compare_recording,
                        ignore,
                        strict_timing,
                        max_verify_duration,
                        verify_start_timeout,
                    },
            },
        ) => {
            run_recording_verify_smoke(
                recording_id,
                target_bmux.as_deref(),
                compare_recording.as_deref(),
                ignore.as_deref(),
                *strict_timing,
                *max_verify_duration,
                *verify_start_timeout,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingExport,
            Command::Recording {
                command:
                    RecordingCommand::Export {
                        recording_id,
                        format,
                        output,
                        view_client,
                        speed,
                        fps,
                        max_duration,
                        max_frames,
                        renderer,
                        cell_size,
                        cell_width,
                        cell_height,
                        font_family,
                        font_size,
                        line_height,
                        font_path,
                        cursor,
                        cursor_shape,
                        cursor_blink,
                        cursor_blink_period_ms,
                        cursor_color,
                        cursor_profile,
                        cursor_solid_after_activity_ms,
                        cursor_solid_after_input_ms,
                        cursor_solid_after_output_ms,
                        cursor_solid_after_cursor_ms,
                        cursor_paint_mode,
                        cursor_text_mode,
                        cursor_bar_width_pct,
                        cursor_underline_height_pct,
                        export_metadata,
                        no_progress,
                    },
            },
        ) => {
            run_recording_export(
                recording_id,
                *format,
                output,
                view_client.as_deref(),
                *speed,
                *fps,
                *max_duration,
                *max_frames,
                *renderer,
                *cell_size,
                *cell_width,
                *cell_height,
                font_family.as_deref(),
                *font_size,
                *line_height,
                font_path,
                *cursor,
                *cursor_shape,
                *cursor_blink,
                *cursor_blink_period_ms,
                cursor_color.as_deref(),
                *cursor_profile,
                *cursor_solid_after_activity_ms,
                *cursor_solid_after_input_ms,
                *cursor_solid_after_output_ms,
                *cursor_solid_after_cursor_ms,
                *cursor_paint_mode,
                *cursor_text_mode,
                *cursor_bar_width_pct,
                *cursor_underline_height_pct,
                export_metadata.as_deref(),
                !*no_progress,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingPrune,
            Command::Recording {
                command: RecordingCommand::Prune { older_than, json },
            },
        ) => recording::run_recording_prune(*older_than, *json).await,
        (
            BuiltInHandlerId::PlaybookRun,
            Command::Playbook {
                command:
                    PlaybookCommand::Run {
                        source,
                        json,
                        target_server,
                        record,
                        export_gif,
                        viewport,
                        timeout,
                        shell,
                        vars,
                        verbose,
                    },
            },
        ) => {
            run_playbook_run(
                source,
                *json,
                *target_server,
                *record,
                export_gif.as_deref(),
                viewport.as_deref(),
                *timeout,
                shell.as_deref(),
                vars,
                *verbose,
            )
            .await
        }
        (
            BuiltInHandlerId::PlaybookValidate,
            Command::Playbook {
                command: PlaybookCommand::Validate { source, json },
            },
        ) => run_playbook_validate(source, *json),
        (
            BuiltInHandlerId::PlaybookInteractive,
            Command::Playbook {
                command:
                    PlaybookCommand::Interactive {
                        socket,
                        record,
                        viewport,
                        shell,
                        timeout,
                    },
            },
        ) => {
            run_playbook_interactive(
                socket.as_deref(),
                *record,
                viewport,
                shell.as_deref(),
                *timeout,
            )
            .await
        }
        (
            BuiltInHandlerId::PlaybookFromRecording,
            Command::Playbook {
                command:
                    PlaybookCommand::FromRecording {
                        recording_id,
                        output,
                    },
            },
        ) => run_playbook_from_recording(recording_id, output.as_deref()),
        (
            BuiltInHandlerId::PlaybookDryRun,
            Command::Playbook {
                command: PlaybookCommand::DryRun { source, json },
            },
        ) => run_playbook_dry_run(source, *json),
        (
            BuiltInHandlerId::PlaybookDiff,
            Command::Playbook {
                command:
                    PlaybookCommand::Diff {
                        left,
                        right,
                        json,
                        timing_threshold,
                    },
            },
        ) => run_playbook_diff(left, right, *json, *timing_threshold),
        (
            BuiltInHandlerId::PlaybookCleanup,
            Command::Playbook {
                command: PlaybookCommand::Cleanup { dry_run, json },
            },
        ) => run_playbook_cleanup(*dry_run, *json),
        _ => unreachable!("built-in command handler and command variant should stay in sync"),
    }
}
