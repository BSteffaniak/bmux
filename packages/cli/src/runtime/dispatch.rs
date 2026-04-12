use anyhow::Result;
use bmux_cli_schema::{
    AccessCommand, AuthCommand, Command, ConfigCommand, ConfigProfilesCommand, KeymapCommand,
    LogsCommand, LogsProfilesCommand, PerfCommand, PlaybookCommand, RecordingCommand,
    RecordingEventKindArg, RemoteCommand, RemoteCompleteCommand, SandboxCommand, SandboxSourceArg,
    SandboxStatusArg, ServerCommand, ServerRecordingCommand, SessionCommand, TerminalCommand,
};
use bmux_config::{BmuxConfig, SandboxCleanupSource};
use bmux_ipc::{RecordingEventKind, RecordingRollingStartOptions};

use super::{
    BuiltInHandlerId, ConnectionContext, RunSandboxOptions, built_in_command_by_handler, recording,
    run_access_add, run_access_disable, run_access_enable, run_access_init, run_access_list,
    run_access_remove, run_access_status, run_auth_login, run_auth_logout, run_auth_status,
    run_client_list, run_config_get, run_config_path, run_config_profiles_diff,
    run_config_profiles_evaluate, run_config_profiles_explain, run_config_profiles_lint,
    run_config_profiles_list, run_config_profiles_resolve, run_config_profiles_show,
    run_config_profiles_switch, run_config_set, run_config_show, run_connect, run_doctor,
    run_external_plugin_command, run_follow, run_host, run_hosts, run_join, run_keymap_doctor,
    run_keymap_explain, run_logs_level, run_logs_path, run_logs_profiles_delete,
    run_logs_profiles_list, run_logs_profiles_rename, run_logs_profiles_show, run_logs_tail,
    run_logs_watch, run_perf_off, run_perf_on, run_perf_status, run_playbook_cleanup,
    run_playbook_diff, run_playbook_dry_run, run_playbook_from_recording, run_playbook_interactive,
    run_playbook_run, run_playbook_validate, run_recording_analyze, run_recording_cut,
    run_recording_delete, run_recording_delete_all, run_recording_export, run_recording_inspect,
    run_recording_list, run_recording_path, run_recording_replay, run_recording_start,
    run_recording_status, run_recording_stop, run_recording_verify_smoke,
    run_remote_complete_sessions, run_remote_complete_targets, run_remote_doctor, run_remote_init,
    run_remote_install_server, run_remote_list, run_remote_test, run_remote_upgrade,
    run_sandbox_bundle, run_sandbox_cleanup, run_sandbox_doctor, run_sandbox_inspect,
    run_sandbox_list, run_sandbox_rebuild_index, run_sandbox_run, run_server_bridge,
    run_server_gateway, run_server_recording_clear, run_server_recording_path,
    run_server_recording_start, run_server_recording_status, run_server_recording_stop,
    run_server_restore, run_server_save, run_server_start, run_server_status, run_server_stop,
    run_server_whoami_principal, run_session_attach, run_session_detach, run_session_kill,
    run_session_kill_all, run_session_list, run_session_new, run_setup, run_share,
    run_terminal_doctor, run_terminal_install_terminfo, run_unfollow, run_unshare,
};

pub(super) async fn run_command(
    command: &Command,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    match command {
        Command::External(args) => run_external_plugin_command(args).await,
        _ => dispatch_built_in_command(command, connection_context).await,
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn built_in_handler_for_command(command: &Command) -> BuiltInHandlerId {
    match command {
        Command::Setup { .. } => BuiltInHandlerId::Setup,
        Command::Host { .. } => BuiltInHandlerId::Host,
        Command::Join { .. } => BuiltInHandlerId::Join,
        Command::Hosts { .. } => BuiltInHandlerId::Hosts,
        Command::Auth { command } => match command {
            AuthCommand::Login { .. } => BuiltInHandlerId::AuthLogin,
            AuthCommand::Status => BuiltInHandlerId::AuthStatus,
            AuthCommand::Logout => BuiltInHandlerId::AuthLogout,
        },
        Command::Access { .. } => BuiltInHandlerId::Access,
        Command::Share { .. } => BuiltInHandlerId::Share,
        Command::Unshare { .. } => BuiltInHandlerId::Unshare,
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
            ServerCommand::Recording { command } => match command {
                ServerRecordingCommand::Start { .. } => BuiltInHandlerId::ServerRecordingStart,
                ServerRecordingCommand::Stop => BuiltInHandlerId::ServerRecordingStop,
                ServerRecordingCommand::Status { .. } => BuiltInHandlerId::ServerRecordingStatus,
                ServerRecordingCommand::Path { .. } => BuiltInHandlerId::ServerRecordingPath,
                ServerRecordingCommand::Clear { .. } => BuiltInHandlerId::ServerRecordingClear,
            },
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
            ConfigCommand::Profiles { command } => match command {
                ConfigProfilesCommand::List { .. } => BuiltInHandlerId::ConfigProfilesList,
                ConfigProfilesCommand::Show { .. } => BuiltInHandlerId::ConfigProfilesShow,
                ConfigProfilesCommand::Resolve { .. } => BuiltInHandlerId::ConfigProfilesResolve,
                ConfigProfilesCommand::Explain { .. } => BuiltInHandlerId::ConfigProfilesExplain,
                ConfigProfilesCommand::Switch { .. } => BuiltInHandlerId::ConfigProfilesSwitch,
                ConfigProfilesCommand::Diff { .. } => BuiltInHandlerId::ConfigProfilesDiff,
                ConfigProfilesCommand::Lint { .. } => BuiltInHandlerId::ConfigProfilesLint,
                ConfigProfilesCommand::Evaluate { .. } => BuiltInHandlerId::ConfigProfilesEvaluate,
            },
        },
        Command::Perf { command } => match command {
            PerfCommand::Status { .. } => BuiltInHandlerId::PerfStatus,
            PerfCommand::On { .. } => BuiltInHandlerId::PerfOn,
            PerfCommand::Off { .. } => BuiltInHandlerId::PerfOff,
        },
        Command::Doctor { .. } => BuiltInHandlerId::Doctor,
        Command::Keymap { command } => match command {
            KeymapCommand::Doctor { .. } => BuiltInHandlerId::KeymapDoctor,
            KeymapCommand::Explain { .. } => BuiltInHandlerId::KeymapExplain,
        },
        Command::Terminal { command } => match command {
            TerminalCommand::Doctor { .. } => BuiltInHandlerId::TerminalDoctor,
            TerminalCommand::InstallTerminfo { .. } => BuiltInHandlerId::TerminalInstallTerminfo,
        },
        Command::Recording { command } => match command {
            RecordingCommand::Start { .. } => BuiltInHandlerId::RecordingStart,
            RecordingCommand::Stop { .. } => BuiltInHandlerId::RecordingStop,
            RecordingCommand::Status { .. } => BuiltInHandlerId::RecordingStatus,
            RecordingCommand::Path { .. } => BuiltInHandlerId::RecordingPath,
            RecordingCommand::List { .. } => BuiltInHandlerId::RecordingList,
            RecordingCommand::Delete { .. } => BuiltInHandlerId::RecordingDelete,
            RecordingCommand::DeleteAll { .. } => BuiltInHandlerId::RecordingDeleteAll,
            RecordingCommand::Cut { .. } => BuiltInHandlerId::RecordingCut,
            RecordingCommand::Inspect { .. } => BuiltInHandlerId::RecordingInspect,
            RecordingCommand::Analyze { .. } => BuiltInHandlerId::RecordingAnalyze,
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
        Command::Sandbox { command } => match command {
            SandboxCommand::Dev { .. } => BuiltInHandlerId::SandboxDev,
            SandboxCommand::Run { .. } => BuiltInHandlerId::SandboxRun,
            SandboxCommand::List { .. } => BuiltInHandlerId::SandboxList,
            SandboxCommand::Inspect { .. } => BuiltInHandlerId::SandboxInspect,
            SandboxCommand::Doctor { .. } => BuiltInHandlerId::SandboxDoctor,
            SandboxCommand::Bundle { .. } => BuiltInHandlerId::SandboxBundle,
            SandboxCommand::Cleanup { .. } => BuiltInHandlerId::SandboxCleanup,
            SandboxCommand::RebuildIndex { .. } => BuiltInHandlerId::SandboxRebuildIndex,
        },
        Command::External(_) => unreachable!("external commands are dispatched separately"),
    }
}

#[allow(
    clippy::match_same_arms,
    clippy::too_many_lines,
    clippy::large_stack_frames
)] // Short aliases and full subcommand forms dispatch to the same function by design
pub(super) async fn dispatch_built_in_command(
    command: &Command,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let handler = built_in_handler_for_command(command);
    let _descriptor = built_in_command_by_handler(handler);
    match (handler, command) {
        (BuiltInHandlerId::Setup, Command::Setup { check, mode }) => run_setup(*check, *mode).await,
        (
            BuiltInHandlerId::Host,
            Command::Host {
                listen,
                name,
                copy,
                daemon,
                status,
                stop,
                restart,
                mode,
            },
        ) => {
            run_host(
                listen,
                name.as_deref(),
                *copy,
                *daemon,
                *status,
                *stop,
                *restart,
                *mode,
                false,
            )
            .await
        }
        (BuiltInHandlerId::Join, Command::Join { link, session }) => {
            run_join(link.as_deref(), session.as_deref()).await
        }
        (BuiltInHandlerId::Hosts, Command::Hosts { verbose }) => run_hosts(*verbose),
        (
            BuiltInHandlerId::AuthLogin,
            Command::Auth {
                command: AuthCommand::Login { no_browser },
            },
        ) => run_auth_login(*no_browser).await,
        (
            BuiltInHandlerId::AuthStatus,
            Command::Auth {
                command: AuthCommand::Status,
            },
        ) => run_auth_status(),
        (
            BuiltInHandlerId::AuthLogout,
            Command::Auth {
                command: AuthCommand::Logout,
            },
        ) => run_auth_logout(),
        (
            BuiltInHandlerId::Access,
            Command::Access {
                command: AccessCommand::Status,
            },
        ) => run_access_status(),
        (
            BuiltInHandlerId::Access,
            Command::Access {
                command:
                    AccessCommand::Init {
                        agent,
                        key_file,
                        public_key,
                        github_user,
                        yes,
                    },
            },
        ) => run_access_init(*agent, key_file, public_key, github_user, *yes),
        (
            BuiltInHandlerId::Access,
            Command::Access {
                command:
                    AccessCommand::Add {
                        agent,
                        key_file,
                        public_key,
                        github_user,
                    },
            },
        ) => run_access_add(*agent, key_file, public_key, github_user),
        (
            BuiltInHandlerId::Access,
            Command::Access {
                command: AccessCommand::List { json },
            },
        ) => run_access_list(*json),
        (
            BuiltInHandlerId::Access,
            Command::Access {
                command: AccessCommand::Remove { fingerprint, yes },
            },
        ) => run_access_remove(fingerprint, *yes),
        (
            BuiltInHandlerId::Access,
            Command::Access {
                command: AccessCommand::Enable,
            },
        ) => run_access_enable(),
        (
            BuiltInHandlerId::Access,
            Command::Access {
                command: AccessCommand::Disable,
            },
        ) => run_access_disable(),
        (
            BuiltInHandlerId::Share,
            Command::Share {
                target,
                secondary,
                name,
                role,
                ttl,
                one_time,
                copy,
                qr,
            },
        ) => {
            run_share(
                target.as_deref(),
                secondary.as_deref(),
                name.as_deref(),
                role,
                ttl.as_deref(),
                *one_time,
                *copy,
                *qr,
            )
            .await
        }
        (BuiltInHandlerId::Unshare, Command::Unshare { name }) => run_unshare(name).await,
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
                target.as_deref(),
                session.as_deref(),
                follow.as_deref(),
                *global,
                *reconnect_forever,
            )
            .await
        }
        (BuiltInHandlerId::NewSession, Command::NewSession { name }) => {
            run_session_new(name.clone(), connection_context).await
        }
        (BuiltInHandlerId::ListSessions, Command::ListSessions { json }) => {
            run_session_list(*json, connection_context).await
        }
        (BuiltInHandlerId::ListClients, Command::ListClients { json }) => {
            run_client_list(*json, connection_context).await
        }
        (
            BuiltInHandlerId::KillSession,
            Command::KillSession {
                target,
                force_local,
            },
        ) => run_session_kill(target, *force_local, connection_context).await,
        (BuiltInHandlerId::KillAllSessions, Command::KillAllSessions { force_local }) => {
            run_session_kill_all(*force_local, connection_context).await
        }
        (
            BuiltInHandlerId::Attach,
            Command::Attach {
                target,
                follow,
                global,
            },
        ) => {
            run_session_attach(
                target.as_deref(),
                follow.as_deref(),
                *global,
                connection_context,
            )
            .await
        }
        (BuiltInHandlerId::Detach, Command::Detach) => run_session_detach(connection_context).await,
        (
            BuiltInHandlerId::Follow,
            Command::Follow {
                target_client_id,
                global,
            },
        ) => run_follow(target_client_id, *global, connection_context).await,
        (BuiltInHandlerId::Unfollow, Command::Unfollow) => run_unfollow(connection_context).await,
        (
            BuiltInHandlerId::SessionNew,
            Command::Session {
                command: SessionCommand::New { name },
            },
        ) => run_session_new(name.clone(), connection_context).await,
        (
            BuiltInHandlerId::SessionList,
            Command::Session {
                command: SessionCommand::List { json },
            },
        ) => run_session_list(*json, connection_context).await,
        (
            BuiltInHandlerId::SessionClients,
            Command::Session {
                command: SessionCommand::Clients { json },
            },
        ) => run_client_list(*json, connection_context).await,
        (
            BuiltInHandlerId::SessionKill,
            Command::Session {
                command:
                    SessionCommand::Kill {
                        target,
                        force_local,
                    },
            },
        ) => run_session_kill(target, *force_local, connection_context).await,
        (
            BuiltInHandlerId::SessionKillAll,
            Command::Session {
                command: SessionCommand::KillAll { force_local },
            },
        ) => run_session_kill_all(*force_local, connection_context).await,
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
        ) => {
            run_session_attach(
                target.as_deref(),
                follow.as_deref(),
                *global,
                connection_context,
            )
            .await
        }
        (
            BuiltInHandlerId::SessionDetach,
            Command::Session {
                command: SessionCommand::Detach,
            },
        ) => run_session_detach(connection_context).await,
        (
            BuiltInHandlerId::SessionFollow,
            Command::Session {
                command:
                    SessionCommand::Follow {
                        target_client_id,
                        global,
                    },
            },
        ) => run_follow(target_client_id, *global, connection_context).await,
        (
            BuiltInHandlerId::SessionUnfollow,
            Command::Session {
                command: SessionCommand::Unfollow,
            },
        ) => run_unfollow(connection_context).await,
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
                        iroh,
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
                iroh.as_deref(),
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
                        pane_shell_integration,
                        no_pane_shell_integration,
                        rolling_recording,
                        no_rolling_recording,
                        rolling_window_secs,
                        rolling_event_kind_all,
                        rolling_event_kind,
                        rolling_capture_input,
                        no_rolling_capture_input,
                        rolling_capture_output,
                        no_rolling_capture_output,
                        rolling_capture_events,
                        no_rolling_capture_events,
                        rolling_capture_protocol_replies,
                        no_rolling_capture_protocol_replies,
                        rolling_capture_images,
                        no_rolling_capture_images,
                    },
            },
        ) => {
            let rolling_enabled_override = if *rolling_recording {
                Some(true)
            } else if *no_rolling_recording {
                Some(false)
            } else {
                None
            };
            let pane_shell_integration_override =
                bool_override(*pane_shell_integration, *no_pane_shell_integration);
            let rolling_options = RecordingRollingStartOptions {
                window_secs: *rolling_window_secs,
                name: None,
                event_kinds: if *rolling_event_kind_all {
                    Some(all_recording_event_kinds())
                } else if rolling_event_kind.is_empty() {
                    None
                } else {
                    Some(
                        rolling_event_kind
                            .iter()
                            .copied()
                            .map(recording_event_kind_arg_to_ipc)
                            .collect(),
                    )
                },
                capture_input: bool_override(*rolling_capture_input, *no_rolling_capture_input),
                capture_output: bool_override(*rolling_capture_output, *no_rolling_capture_output),
                capture_events: bool_override(*rolling_capture_events, *no_rolling_capture_events),
                capture_protocol_replies: bool_override(
                    *rolling_capture_protocol_replies,
                    *no_rolling_capture_protocol_replies,
                ),
                capture_images: bool_override(*rolling_capture_images, *no_rolling_capture_images),
            };
            run_server_start(
                *daemon,
                *foreground_internal,
                rolling_enabled_override,
                rolling_options,
                pane_shell_integration_override,
            )
            .await
        }
        (
            BuiltInHandlerId::ServerStatus,
            Command::Server {
                command: ServerCommand::Status { json },
            },
        ) => run_server_status(*json, connection_context).await,
        (
            BuiltInHandlerId::ServerWhoamiPrincipal,
            Command::Server {
                command: ServerCommand::WhoamiPrincipal { json },
            },
        ) => run_server_whoami_principal(*json, connection_context).await,
        (
            BuiltInHandlerId::ServerSave,
            Command::Server {
                command: ServerCommand::Save,
            },
        ) => run_server_save(connection_context).await,
        (
            BuiltInHandlerId::ServerRestore,
            Command::Server {
                command: ServerCommand::Restore { dry_run, yes },
            },
        ) => run_server_restore(*dry_run, *yes, connection_context).await,
        (
            BuiltInHandlerId::ServerStop,
            Command::Server {
                command: ServerCommand::Stop,
            },
        ) => run_server_stop(connection_context).await,
        (
            BuiltInHandlerId::ServerRecordingStart,
            Command::Server {
                command:
                    ServerCommand::Recording {
                        command:
                            ServerRecordingCommand::Start {
                                rolling_window_secs,
                                name,
                                rolling_event_kind_all,
                                rolling_event_kind,
                                rolling_capture_input,
                                no_rolling_capture_input,
                                rolling_capture_output,
                                no_rolling_capture_output,
                                rolling_capture_events,
                                no_rolling_capture_events,
                                rolling_capture_protocol_replies,
                                no_rolling_capture_protocol_replies,
                                rolling_capture_images,
                                no_rolling_capture_images,
                            },
                    },
            },
        ) => {
            run_server_recording_start(
                RecordingRollingStartOptions {
                    window_secs: *rolling_window_secs,
                    name: name.clone(),
                    event_kinds: if *rolling_event_kind_all {
                        Some(all_recording_event_kinds())
                    } else if rolling_event_kind.is_empty() {
                        None
                    } else {
                        Some(
                            rolling_event_kind
                                .iter()
                                .copied()
                                .map(recording_event_kind_arg_to_ipc)
                                .collect(),
                        )
                    },
                    capture_input: bool_override(*rolling_capture_input, *no_rolling_capture_input),
                    capture_output: bool_override(
                        *rolling_capture_output,
                        *no_rolling_capture_output,
                    ),
                    capture_events: bool_override(
                        *rolling_capture_events,
                        *no_rolling_capture_events,
                    ),
                    capture_protocol_replies: bool_override(
                        *rolling_capture_protocol_replies,
                        *no_rolling_capture_protocol_replies,
                    ),
                    capture_images: bool_override(
                        *rolling_capture_images,
                        *no_rolling_capture_images,
                    ),
                },
                connection_context,
            )
            .await
        }
        (
            BuiltInHandlerId::ServerRecordingStop,
            Command::Server {
                command:
                    ServerCommand::Recording {
                        command: ServerRecordingCommand::Stop,
                    },
            },
        ) => run_server_recording_stop(connection_context).await,
        (
            BuiltInHandlerId::ServerRecordingStatus,
            Command::Server {
                command:
                    ServerCommand::Recording {
                        command: ServerRecordingCommand::Status { json },
                    },
            },
        ) => run_server_recording_status(*json, connection_context).await,
        (
            BuiltInHandlerId::ServerRecordingPath,
            Command::Server {
                command:
                    ServerCommand::Recording {
                        command: ServerRecordingCommand::Path { json },
                    },
            },
        ) => run_server_recording_path(*json, connection_context).await,
        (
            BuiltInHandlerId::ServerRecordingClear,
            Command::Server {
                command:
                    ServerCommand::Recording {
                        command: ServerRecordingCommand::Clear { json, no_restart },
                    },
            },
        ) => run_server_recording_clear(*json, *no_restart, connection_context).await,
        (
            BuiltInHandlerId::ServerGateway,
            Command::Server {
                command:
                    ServerCommand::Gateway {
                        listen,
                        host,
                        host_mode,
                        host_relay,
                        quick,
                        cert_file,
                        key_file,
                    },
            },
        ) => {
            run_server_gateway(
                listen,
                *host,
                *host_mode,
                host_relay,
                *quick,
                cert_file.as_deref(),
                key_file.as_deref(),
            )
            .await
        }
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
        (
            BuiltInHandlerId::ConfigProfilesList,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command: ConfigProfilesCommand::List { json },
                    },
            },
        ) => run_config_profiles_list(*json),
        (
            BuiltInHandlerId::ConfigProfilesShow,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command: ConfigProfilesCommand::Show { profile, json },
                    },
            },
        ) => run_config_profiles_show(profile, *json),
        (
            BuiltInHandlerId::ConfigProfilesResolve,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command: ConfigProfilesCommand::Resolve { profile, json },
                    },
            },
        ) => run_config_profiles_resolve(profile.as_deref(), *json),
        (
            BuiltInHandlerId::ConfigProfilesExplain,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command: ConfigProfilesCommand::Explain { profile, json },
                    },
            },
        ) => run_config_profiles_explain(profile.as_deref(), *json),
        (
            BuiltInHandlerId::ConfigProfilesSwitch,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command:
                            ConfigProfilesCommand::Switch {
                                profile,
                                dry_run,
                                json,
                            },
                    },
            },
        ) => run_config_profiles_switch(profile, *dry_run, *json),
        (
            BuiltInHandlerId::ConfigProfilesDiff,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command: ConfigProfilesCommand::Diff { from, to, json },
                    },
            },
        ) => run_config_profiles_diff(from, to, *json),
        (
            BuiltInHandlerId::ConfigProfilesLint,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command: ConfigProfilesCommand::Lint { json },
                    },
            },
        ) => run_config_profiles_lint(*json),
        (
            BuiltInHandlerId::ConfigProfilesEvaluate,
            Command::Config {
                command:
                    ConfigCommand::Profiles {
                        command: ConfigProfilesCommand::Evaluate { json },
                    },
            },
        ) => run_config_profiles_evaluate(*json),
        (
            BuiltInHandlerId::PerfStatus,
            Command::Perf {
                command: PerfCommand::Status { json },
            },
        ) => run_perf_status(*json, connection_context).await,
        (
            BuiltInHandlerId::PerfOn,
            Command::Perf {
                command: PerfCommand::On { profile, json },
            },
        ) => run_perf_on(*profile, *json, connection_context).await,
        (
            BuiltInHandlerId::PerfOff,
            Command::Perf {
                command: PerfCommand::Off { json },
            },
        ) => run_perf_off(*json, connection_context).await,
        (BuiltInHandlerId::Doctor, Command::Doctor { json, hosted }) => {
            run_doctor(*json, *hosted).await
        }
        (
            BuiltInHandlerId::KeymapDoctor,
            Command::Keymap {
                command: KeymapCommand::Doctor { json },
            },
        ) => run_keymap_doctor(*json),
        (
            BuiltInHandlerId::KeymapExplain,
            Command::Keymap {
                command: KeymapCommand::Explain { key, mode, json },
            },
        ) => run_keymap_explain(key, mode.as_deref(), *json),
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
                        name,
                        profile,
                        event_kind,
                    },
            },
        ) => {
            run_recording_start(
                session_id.as_deref(),
                !*no_capture_input,
                name.as_deref(),
                *profile,
                event_kind,
                connection_context,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingStop,
            Command::Recording {
                command: RecordingCommand::Stop { recording_id },
            },
        ) => run_recording_stop(recording_id.as_deref(), connection_context).await,
        (
            BuiltInHandlerId::RecordingStatus,
            Command::Recording {
                command: RecordingCommand::Status { json },
            },
        ) => run_recording_status(*json, connection_context).await,
        (
            BuiltInHandlerId::RecordingPath,
            Command::Recording {
                command: RecordingCommand::Path { json },
            },
        ) => run_recording_path(*json),
        (
            BuiltInHandlerId::RecordingList,
            Command::Recording {
                command:
                    RecordingCommand::List {
                        json,
                        limit,
                        all,
                        sort,
                        order,
                        status,
                        query,
                    },
            },
        ) => {
            run_recording_list(
                *json,
                recording::RecordingListOptions {
                    limit: *limit,
                    all: *all,
                    sort: *sort,
                    order: *order,
                    status: *status,
                    query: query.as_deref(),
                },
                connection_context,
            )
            .await
        }
        (
            BuiltInHandlerId::RecordingDelete,
            Command::Recording {
                command: RecordingCommand::Delete { recording_id },
            },
        ) => run_recording_delete(recording_id, connection_context).await,
        (
            BuiltInHandlerId::RecordingDeleteAll,
            Command::Recording {
                command: RecordingCommand::DeleteAll { yes },
            },
        ) => run_recording_delete_all(*yes, connection_context).await,
        (
            BuiltInHandlerId::RecordingCut,
            Command::Recording {
                command: RecordingCommand::Cut { last_seconds, name },
            },
        ) => run_recording_cut(*last_seconds, name.as_deref(), connection_context).await,
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
            BuiltInHandlerId::RecordingAnalyze,
            Command::Recording {
                command:
                    RecordingCommand::Analyze {
                        recording_id,
                        perf,
                        json,
                    },
            },
        ) => run_recording_analyze(recording_id, *perf, *json),
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
                        palette_source,
                        palette_foreground,
                        palette_background,
                        palette_color,
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
                *palette_source,
                palette_foreground.as_deref(),
                palette_background.as_deref(),
                palette_color,
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
        ) => recording::run_recording_prune(*older_than, *json, connection_context).await,
        (
            BuiltInHandlerId::PlaybookRun,
            Command::Playbook {
                command:
                    PlaybookCommand::Run {
                        source,
                        json,
                        interactive,
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
                *interactive,
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
        (
            BuiltInHandlerId::SandboxDev,
            Command::Sandbox {
                command:
                    SandboxCommand::Dev {
                        bmux_bin,
                        env_mode,
                        json,
                        print_env,
                        timeout,
                        name,
                        command,
                    },
            },
        ) => {
            let dev_bmux_bin = default_dev_bmux_bin();
            let options = RunSandboxOptions {
                bmux_bin: bmux_bin.as_deref().or(dev_bmux_bin.as_deref()),
                env_mode: *env_mode,
                keep: true,
                json: *json,
                print_env: *print_env,
                timeout_secs: *timeout,
                name: name.as_deref(),
            };
            run_sandbox_run(options, command).await
        }
        (
            BuiltInHandlerId::SandboxRun,
            Command::Sandbox {
                command:
                    SandboxCommand::Run {
                        bmux_bin,
                        env_mode,
                        keep,
                        json,
                        print_env,
                        timeout,
                        name,
                        command,
                    },
            },
        ) => {
            let options = RunSandboxOptions {
                bmux_bin: bmux_bin.as_deref(),
                env_mode: *env_mode,
                keep: *keep,
                json: *json,
                print_env: *print_env,
                timeout_secs: *timeout,
                name: name.as_deref(),
            };
            run_sandbox_run(options, command).await
        }
        (
            BuiltInHandlerId::SandboxList,
            Command::Sandbox {
                command:
                    SandboxCommand::List {
                        status,
                        source,
                        limit,
                        json,
                    },
            },
        ) => {
            let status_filter = match status {
                SandboxStatusArg::Running => Some("running"),
                SandboxStatusArg::Stopped => Some("stopped"),
                SandboxStatusArg::Failed => Some("failed"),
                SandboxStatusArg::All => None,
            };
            let source_filter = match source {
                SandboxSourceArg::SandboxCli => Some("sandbox-cli"),
                SandboxSourceArg::Playbook => Some("playbook"),
                SandboxSourceArg::RecordingVerify => Some("recording-verify"),
                SandboxSourceArg::All => None,
            };
            run_sandbox_list(status_filter, source_filter, *limit, *json)
        }
        (
            BuiltInHandlerId::SandboxInspect,
            Command::Sandbox {
                command:
                    SandboxCommand::Inspect {
                        sandbox,
                        latest,
                        latest_failed,
                        source,
                        tail,
                        json,
                    },
            },
        ) => {
            let source_filter = match source {
                SandboxSourceArg::SandboxCli => Some("sandbox-cli"),
                SandboxSourceArg::Playbook => Some("playbook"),
                SandboxSourceArg::RecordingVerify => Some("recording-verify"),
                SandboxSourceArg::All => None,
            };
            run_sandbox_inspect(
                sandbox.as_deref(),
                *latest,
                *latest_failed,
                source_filter,
                *tail,
                *json,
            )
        }
        (
            BuiltInHandlerId::SandboxDoctor,
            Command::Sandbox {
                command: SandboxCommand::Doctor { id, json },
            },
        ) => run_sandbox_doctor(id.as_deref(), *json),
        (
            BuiltInHandlerId::SandboxBundle,
            Command::Sandbox {
                command:
                    SandboxCommand::Bundle {
                        sandbox,
                        output,
                        json,
                    },
            },
        ) => run_sandbox_bundle(sandbox, output.as_deref(), *json),
        (
            BuiltInHandlerId::SandboxCleanup,
            Command::Sandbox {
                command:
                    SandboxCommand::Cleanup {
                        dry_run,
                        failed_only,
                        all_status,
                        older_than,
                        source,
                        json,
                    },
            },
        ) => {
            let config = BmuxConfig::load().unwrap_or_default();
            let defaults = config.sandbox.cleanup;

            let resolved_failed_only =
                bool_override(*failed_only, *all_status).unwrap_or(defaults.failed_only);
            let resolved_older_than = older_than.unwrap_or(defaults.older_than_secs);

            let source_arg = source.as_ref().copied().unwrap_or(match defaults.source {
                SandboxCleanupSource::SandboxCli => SandboxSourceArg::SandboxCli,
                SandboxCleanupSource::Playbook => SandboxSourceArg::Playbook,
                SandboxCleanupSource::RecordingVerify => SandboxSourceArg::RecordingVerify,
                SandboxCleanupSource::All => SandboxSourceArg::All,
            });
            let source_filter = match source_arg {
                SandboxSourceArg::SandboxCli => Some("sandbox-cli"),
                SandboxSourceArg::Playbook => Some("playbook"),
                SandboxSourceArg::RecordingVerify => Some("recording-verify"),
                SandboxSourceArg::All => None,
            };

            run_sandbox_cleanup(
                *dry_run,
                resolved_failed_only,
                Some(resolved_older_than),
                source_filter,
                *json,
            )
        }
        (
            BuiltInHandlerId::SandboxRebuildIndex,
            Command::Sandbox {
                command: SandboxCommand::RebuildIndex { json },
            },
        ) => run_sandbox_rebuild_index(*json),
        _ => unreachable!("built-in command handler and command variant should stay in sync"),
    }
}

fn default_dev_bmux_bin() -> Option<String> {
    let candidate = std::path::Path::new("target").join("debug").join("bmux");
    candidate
        .exists()
        .then(|| candidate.to_string_lossy().to_string())
}

const fn bool_override(positive: bool, negative: bool) -> Option<bool> {
    if positive {
        Some(true)
    } else if negative {
        Some(false)
    } else {
        None
    }
}

fn all_recording_event_kinds() -> Vec<RecordingEventKind> {
    vec![
        RecordingEventKind::PaneInputRaw,
        RecordingEventKind::PaneOutputRaw,
        RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKind::PaneImage,
        RecordingEventKind::ServerEvent,
        RecordingEventKind::RequestStart,
        RecordingEventKind::RequestDone,
        RecordingEventKind::RequestError,
        RecordingEventKind::Custom,
    ]
}

const fn recording_event_kind_arg_to_ipc(kind: RecordingEventKindArg) -> RecordingEventKind {
    match kind {
        RecordingEventKindArg::PaneInputRaw => RecordingEventKind::PaneInputRaw,
        RecordingEventKindArg::PaneOutputRaw => RecordingEventKind::PaneOutputRaw,
        RecordingEventKindArg::ProtocolReplyRaw => RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKindArg::PaneImage => RecordingEventKind::PaneImage,
        RecordingEventKindArg::ServerEvent => RecordingEventKind::ServerEvent,
        RecordingEventKindArg::RequestStart => RecordingEventKind::RequestStart,
        RecordingEventKindArg::RequestDone => RecordingEventKind::RequestDone,
        RecordingEventKindArg::RequestError => RecordingEventKind::RequestError,
        RecordingEventKindArg::Custom => RecordingEventKind::Custom,
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[test]
    fn built_in_handler_maps_config_profiles_explain() {
        let command = Command::Config {
            command: ConfigCommand::Profiles {
                command: ConfigProfilesCommand::Explain {
                    profile: Some("vim".to_string()),
                    json: false,
                },
            },
        };
        assert_eq!(
            built_in_handler_for_command(&command),
            BuiltInHandlerId::ConfigProfilesExplain
        );
    }

    #[test]
    fn built_in_handler_maps_config_profiles_switch_with_dry_run() {
        let command = Command::Config {
            command: ConfigCommand::Profiles {
                command: ConfigProfilesCommand::Switch {
                    profile: "tmux_compat".to_string(),
                    dry_run: true,
                    json: true,
                },
            },
        };
        assert_eq!(
            built_in_handler_for_command(&command),
            BuiltInHandlerId::ConfigProfilesSwitch
        );
    }

    #[test]
    fn built_in_handler_maps_keymap_explain() {
        let command = Command::Keymap {
            command: KeymapCommand::Explain {
                key: "ctrl+b n".to_string(),
                mode: Some("normal".to_string()),
                json: true,
            },
        };
        assert_eq!(
            built_in_handler_for_command(&command),
            BuiltInHandlerId::KeymapExplain
        );
    }
}
