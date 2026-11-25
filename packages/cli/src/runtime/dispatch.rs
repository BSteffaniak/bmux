use anyhow::Result;
use bmux_cli_schema::{
    Command, ConfigCommand, ConfigProfilesCommand, ConfigScopeCommand, KeymapCommand, LogsCommand,
    RecordingEventKindArg, SandboxCommand, SandboxEnvModeArg, SandboxSourceArg, SandboxStatusArg,
    ServerAutostartCommand, ServerCommand, SlotCommand, TerminalCommand,
};
use bmux_config::{BmuxConfig, SandboxCleanupSource};
use bmux_recording_protocol::{RecordingEventKind, RecordingRollingStartOptions};

use super::{
    BuiltInHandlerId, BundleIncludeOptions, BundleSandboxOptions, ConnectionContext,
    InspectTargetOptions, RerunSandboxOptions, RunSandboxOptions, TriageBundleOptions,
    TriageSandboxOptions, built_in_command_by_handler, run_config_get, run_config_path,
    run_config_profiles_diff, run_config_profiles_evaluate, run_config_profiles_explain,
    run_config_profiles_lint, run_config_profiles_list, run_config_profiles_resolve,
    run_config_profiles_show, run_config_profiles_switch, run_config_scope_explain, run_config_set,
    run_config_show, run_device_seal_broker, run_doctor, run_external_plugin_command,
    run_keymap_doctor, run_keymap_explain, run_logs_level, run_logs_path, run_sandbox_bundle,
    run_sandbox_cleanup, run_sandbox_doctor, run_sandbox_inspect, run_sandbox_list,
    run_sandbox_open, run_sandbox_rebuild_index, run_sandbox_rerun, run_sandbox_run,
    run_sandbox_status, run_sandbox_tail, run_sandbox_triage, run_sandbox_verify_bundle,
    run_server_start, run_server_status, run_server_stop, run_server_whoami_principal,
    run_session_attach, run_terminal_doctor, run_terminal_install_terminfo,
    server_autostart::{
        run_server_autostart_install, run_server_autostart_print, run_server_autostart_status,
        run_server_autostart_uninstall,
    },
};

// Inspect the concrete future type without calling the factory or constructing
// its potentially large debug-mode stack temporary.
pub(super) fn future_size<F: std::future::Future>(_: impl FnOnce() -> F) -> usize {
    std::mem::size_of::<F>()
}

pub fn startup_future_marker(label: &str, bytes: usize) {
    if std::env::var_os("BMUX_DIAG_STARTUP_FUTURES").is_some() {
        eprintln!("startup.future {label} bytes={bytes}");
    }
}

#[cfg(test)]
#[test]
fn startup_future_size_diagnostic() {
    // Retain poll symbols for offline prologue inspection without executing CLI
    // parsing, opening endpoints, or starting a runtime.
    fn retain_poll<F: std::future::Future>(_: impl FnOnce() -> F) {
        fn poll<F: std::future::Future>(
            future: std::pin::Pin<&mut F>,
            context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<F::Output> {
            future.poll(context)
        }
        std::hint::black_box(
            poll::<F>
                as fn(
                    std::pin::Pin<&mut F>,
                    &mut std::task::Context<'_>,
                ) -> std::task::Poll<F::Output>,
        );
    }
    retain_poll(|| super::run(None));
    retain_poll(crate::run_cli);
    let command = Command::Server {
        command: ServerCommand::Status { json: false },
    };
    let context = ConnectionContext::new(Some("local"));
    eprintln!(
        "dispatch bytes={}",
        future_size(|| dispatch_built_in_command(&command, context, None))
    );
    let server_dispatch_bytes = future_size(|| dispatch_server_start(&command, None));
    eprintln!("server_dispatch bytes={server_dispatch_bytes}");
    assert!(
        server_dispatch_bytes < 1024,
        "server dispatch must not embed the general dispatcher future"
    );
    eprintln!("runtime.run bytes={}", future_size(|| super::run(None)));
    eprintln!(
        "run_command bytes={}",
        future_size(|| run_command(&command, context, None))
    );
}

// The monolithic dispatcher has a >1 MiB debug poll frame even though its
// stored future is small. Keep dedicated server startup out of that poll frame.
async fn dispatch_server_start(
    command: &Command,
    runtime_owner: Option<crate::runtime_owner::RuntimeOwner>,
) -> Result<u8> {
    match (BuiltInHandlerId::ServerStart, command) {
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
                None,
                runtime_owner,
            )
            .await
        }
        _ => anyhow::bail!("server-start dispatcher received another command"),
    }
}

/// A foreground server loads plugins under this process's explicitly selected
/// runtime owner; nothing rediscovers it from task-local or thread context.
fn selected_runtime_owner(
    command_state: Option<&super::plugin_runtime::RuntimeCommandState>,
) -> Option<crate::runtime_owner::RuntimeOwner> {
    command_state
        .and_then(|state| state.runtime_owner.as_ref())
        .and_then(crate::runtime_owner::WeakRuntimeOwner::upgrade)
}

pub(super) async fn run_command(
    command: &Command,
    connection_context: ConnectionContext<'_>,
    command_state: Option<&super::plugin_runtime::RuntimeCommandState>,
) -> Result<u8> {
    startup_future_marker("run_command.first_poll", 0);
    startup_future_marker(
        "dispatch.before_construct",
        future_size(|| dispatch_built_in_command(command, connection_context, command_state)),
    );
    match command {
        Command::External(args) => run_external_plugin_command(args).await,
        Command::Server {
            command: ServerCommand::Start { .. },
        } => {
            let _descriptor = built_in_command_by_handler(BuiltInHandlerId::ServerStart);
            let runtime_owner = selected_runtime_owner(command_state);
            startup_future_marker(
                "server_dispatch.before_construct",
                future_size(|| dispatch_server_start(command, runtime_owner.clone())),
            );
            Box::pin(dispatch_server_start(command, runtime_owner)).await
        }
        _ => {
            Box::pin(dispatch_built_in_command(
                command,
                connection_context,
                command_state,
            ))
            .await
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn built_in_handler_for_command(command: &Command) -> BuiltInHandlerId {
    match command {
        Command::Attach { .. } => BuiltInHandlerId::Attach,
        Command::Server { command } => match command {
            ServerCommand::Start { .. } => BuiltInHandlerId::ServerStart,
            ServerCommand::Status { .. } => BuiltInHandlerId::ServerStatus,
            ServerCommand::WhoamiPrincipal { .. } => BuiltInHandlerId::ServerWhoamiPrincipal,
            ServerCommand::Stop => BuiltInHandlerId::ServerStop,
            ServerCommand::Autostart { command } => match command {
                ServerAutostartCommand::Install { .. } => BuiltInHandlerId::ServerAutostartInstall,
                ServerAutostartCommand::Uninstall => BuiltInHandlerId::ServerAutostartUninstall,
                ServerAutostartCommand::Status { .. } => BuiltInHandlerId::ServerAutostartStatus,
                ServerAutostartCommand::Print { .. } => BuiltInHandlerId::ServerAutostartPrint,
            },
        },
        Command::Logs { command } => match command {
            LogsCommand::Path { .. } => BuiltInHandlerId::LogsPath,
            LogsCommand::Level { .. } => BuiltInHandlerId::LogsLevel,
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
            ConfigCommand::Scope { command } => match command {
                ConfigScopeCommand::Explain { .. } => BuiltInHandlerId::ConfigScopeExplain,
            },
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
        Command::Sandbox { command } => match command {
            SandboxCommand::Dev { .. } => BuiltInHandlerId::SandboxDev,
            SandboxCommand::Attach { .. } => BuiltInHandlerId::SandboxAttach,
            SandboxCommand::Stop { .. } => BuiltInHandlerId::SandboxStop,
            SandboxCommand::Run { .. } => BuiltInHandlerId::SandboxRun,
            SandboxCommand::List { .. } => BuiltInHandlerId::SandboxList,
            SandboxCommand::Status { .. } => BuiltInHandlerId::SandboxStatus,
            SandboxCommand::Inspect { .. } => BuiltInHandlerId::SandboxInspect,
            SandboxCommand::Tail { .. } => BuiltInHandlerId::SandboxTail,
            SandboxCommand::Open { .. } => BuiltInHandlerId::SandboxOpen,
            SandboxCommand::Rerun { .. } => BuiltInHandlerId::SandboxRerun,
            SandboxCommand::Triage { .. } => BuiltInHandlerId::SandboxTriage,
            SandboxCommand::Doctor { .. } => BuiltInHandlerId::SandboxDoctor,
            SandboxCommand::Bundle { .. } => BuiltInHandlerId::SandboxBundle,
            SandboxCommand::VerifyBundle { .. } => BuiltInHandlerId::SandboxVerifyBundle,
            SandboxCommand::Cleanup { .. } => BuiltInHandlerId::SandboxCleanup,
            SandboxCommand::Clean { .. } => BuiltInHandlerId::SandboxClean,
            SandboxCommand::RebuildIndex { .. } => BuiltInHandlerId::SandboxRebuildIndex,
        },
        Command::DeviceSealBroker => BuiltInHandlerId::DeviceSealBroker,
        Command::Slot { command } | Command::Env { command } => match command {
            SlotCommand::List { .. } => BuiltInHandlerId::SlotList,
            SlotCommand::Show { .. } => BuiltInHandlerId::SlotShow,
            SlotCommand::Paths { .. } => BuiltInHandlerId::SlotPaths,
            SlotCommand::Doctor => BuiltInHandlerId::SlotDoctor,
            SlotCommand::Install { .. } => BuiltInHandlerId::SlotInstall,
            SlotCommand::Uninstall { .. } => BuiltInHandlerId::SlotUninstall,
            SlotCommand::Shell { .. } => BuiltInHandlerId::SlotShell,
            SlotCommand::Exec { .. } => BuiltInHandlerId::SlotExec,
            SlotCommand::Print { .. } => BuiltInHandlerId::SlotPrint,
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
    command_state: Option<&super::plugin_runtime::RuntimeCommandState>,
) -> Result<u8> {
    startup_future_marker("dispatch.first_poll", 0);
    let handler = built_in_handler_for_command(command);
    let _descriptor = built_in_command_by_handler(handler);
    match (handler, command) {
        (
            BuiltInHandlerId::Attach,
            Command::Attach {
                target,
                follow,
                global,
                observer_socket,
                observer_cols,
                observer_rows,
                web,
                web_assets,
                web_only,
                web_port,
                web_auth,
                web_host,
                web_public_host,
                web_allowed_hosts,
            },
        ) => {
            if let Some(socket) = observer_socket {
                let session_id = target
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("observer requires a session UUID"))?
                    .parse::<uuid::Uuid>()?;
                let client = super::connect_with_context(
                    super::ConnectionPolicyScope::Normal,
                    "bmux-retained-observer",
                    connection_context,
                )
                .await?;
                return super::attach::observer::transport::run_private_worker(
                    client,
                    session_id,
                    socket,
                    *observer_cols,
                    *observer_rows,
                )
                .await;
            }
            if *web || *web_only || web_assets.is_some() {
                return Box::pin(super::web_attach::run(
                    target.as_deref(),
                    follow.as_deref(),
                    *global,
                    web_assets.as_deref(),
                    *web_only,
                    super::web_attach::WebOptions {
                        port: *web_port,
                        auth: web_auth.as_deref(),
                        host: *web_host,
                        public_host: *web_public_host,
                        allowed_hosts: web_allowed_hosts.as_deref(),
                    },
                    connection_context,
                ))
                .await;
            }
            // Attach is baseline core behavior: it must work without the
            // sessions plugin, whose `session-attach` calls this same function.
            Box::pin(run_session_attach(
                target.as_deref(),
                follow.as_deref(),
                *global,
                connection_context,
            ))
            .await
        }
        (BuiltInHandlerId::ServerStart, _) => {
            dispatch_server_start(command, selected_runtime_owner(command_state)).await
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
            BuiltInHandlerId::ServerStop,
            Command::Server {
                command: ServerCommand::Stop,
            },
        ) => run_server_stop(connection_context).await,
        (
            BuiltInHandlerId::ServerAutostartInstall,
            Command::Server {
                command:
                    ServerCommand::Autostart {
                        command:
                            ServerAutostartCommand::Install {
                                no_start,
                                executable,
                            },
                    },
            },
        ) => {
            run_server_autostart_install(*no_start, executable.as_deref(), connection_context).await
        }
        (
            BuiltInHandlerId::ServerAutostartUninstall,
            Command::Server {
                command:
                    ServerCommand::Autostart {
                        command: ServerAutostartCommand::Uninstall,
                    },
            },
        ) => run_server_autostart_uninstall(connection_context),
        (
            BuiltInHandlerId::ServerAutostartStatus,
            Command::Server {
                command:
                    ServerCommand::Autostart {
                        command: ServerAutostartCommand::Status { json },
                    },
            },
        ) => run_server_autostart_status(*json, connection_context).await,
        (
            BuiltInHandlerId::ServerAutostartPrint,
            Command::Server {
                command:
                    ServerCommand::Autostart {
                        command: ServerAutostartCommand::Print { executable },
                    },
            },
        ) => run_server_autostart_print(executable.as_deref(), connection_context),
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
            BuiltInHandlerId::ConfigScopeExplain,
            Command::Config {
                command:
                    ConfigCommand::Scope {
                        command: ConfigScopeCommand::Explain { scope, cwd, json },
                    },
            },
        ) => run_config_scope_explain(scope, cwd.as_deref(), *json),
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
        (BuiltInHandlerId::Doctor, Command::Doctor { json }) => run_doctor(*json).await,
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
            BuiltInHandlerId::SandboxDev,
            Command::Sandbox {
                command:
                    SandboxCommand::Dev {
                        inherit_config,
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
            let options = RunSandboxOptions {
                bmux_bin: bmux_bin.as_deref(),
                env_mode: *env_mode,
                keep: true,
                json: *json,
                print_env: *print_env,
                timeout_secs: *timeout,
                name: name.as_deref(),
            };
            if *inherit_config {
                super::sandbox_cli::run_inherited_sandbox(options, command).await
            } else {
                run_sandbox_run(options, command).await
            }
        }
        (
            BuiltInHandlerId::SandboxAttach,
            Command::Sandbox {
                command: SandboxCommand::Attach { sandbox },
            },
        ) => super::sandbox_cli::run_inherited_sandbox_control(sandbox, false).await,
        (
            BuiltInHandlerId::SandboxStop,
            Command::Sandbox {
                command: SandboxCommand::Stop { sandbox },
            },
        ) => super::sandbox_cli::run_inherited_sandbox_control(sandbox, true).await,
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
            BuiltInHandlerId::SandboxStatus,
            Command::Sandbox {
                command: SandboxCommand::Status { json },
            },
        ) => run_sandbox_status(*json),
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
            let source_filter = sandbox_source_filter(*source);
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
            BuiltInHandlerId::SandboxTail,
            Command::Sandbox {
                command:
                    SandboxCommand::Tail {
                        sandbox,
                        latest,
                        latest_failed,
                        source,
                        tail,
                        json,
                    },
            },
        ) => {
            let source_filter = sandbox_source_filter(*source);
            run_sandbox_tail(
                sandbox.as_deref(),
                *latest,
                *latest_failed,
                source_filter,
                *tail,
                *json,
            )
        }
        (
            BuiltInHandlerId::SandboxOpen,
            Command::Sandbox {
                command:
                    SandboxCommand::Open {
                        sandbox,
                        latest,
                        latest_failed,
                        source,
                        json,
                    },
            },
        ) => {
            let source_filter = sandbox_source_filter(*source);
            run_sandbox_open(
                sandbox.as_deref(),
                *latest,
                *latest_failed,
                source_filter,
                *json,
            )
        }
        (
            BuiltInHandlerId::SandboxRerun,
            Command::Sandbox {
                command:
                    SandboxCommand::Rerun {
                        sandbox,
                        latest,
                        latest_failed,
                        source,
                        bmux_bin,
                        env_mode,
                        keep,
                        json,
                        print_env,
                        timeout,
                        name,
                    },
            },
        ) => {
            let source_filter = sandbox_source_filter(*source);
            run_sandbox_rerun(RerunSandboxOptions {
                inspect: InspectTargetOptions {
                    target: sandbox.as_deref(),
                    latest: *latest,
                    latest_failed: *latest_failed,
                    source_filter,
                },
                run: RunSandboxOptions {
                    bmux_bin: None,
                    env_mode: SandboxEnvModeArg::Inherit,
                    keep: *keep,
                    json: *json,
                    print_env: *print_env,
                    timeout_secs: *timeout,
                    name: name.as_deref(),
                },
                bmux_bin_override: bmux_bin.as_deref(),
                env_mode_override: *env_mode,
            })
            .await
        }
        (
            BuiltInHandlerId::SandboxTriage,
            Command::Sandbox {
                command:
                    SandboxCommand::Triage {
                        sandbox,
                        latest,
                        latest_failed,
                        source,
                        tail,
                        rerun,
                        bmux_bin,
                        env_mode,
                        keep,
                        print_env,
                        timeout,
                        name,
                        bundle,
                        bundle_output,
                        bundle_strict_verify,
                        json,
                    },
            },
        ) => {
            let source_filter = sandbox_source_filter(*source);
            run_sandbox_triage(TriageSandboxOptions {
                inspect: InspectTargetOptions {
                    target: sandbox.as_deref(),
                    latest: *latest,
                    latest_failed: *latest_failed,
                    source_filter,
                },
                tail: *tail,
                rerun: *rerun,
                run: RunSandboxOptions {
                    bmux_bin: None,
                    env_mode: SandboxEnvModeArg::Inherit,
                    keep: *keep,
                    json: false,
                    print_env: *print_env,
                    timeout_secs: *timeout,
                    name: name.as_deref(),
                },
                bmux_bin_override: bmux_bin.as_deref(),
                env_mode_override: *env_mode,
                bundle: bundle.then_some(TriageBundleOptions {
                    output: bundle_output.as_deref(),
                    strict_verify: *bundle_strict_verify,
                }),
                json: *json,
            })
            .await
        }
        (
            BuiltInHandlerId::SandboxDoctor,
            Command::Sandbox {
                command:
                    SandboxCommand::Doctor {
                        id,
                        fix,
                        dry_run,
                        json,
                    },
            },
        ) => run_sandbox_doctor(id.as_deref(), *fix, *dry_run, *json),
        (
            BuiltInHandlerId::SandboxBundle,
            Command::Sandbox {
                command:
                    SandboxCommand::Bundle {
                        sandbox,
                        output,
                        include_env,
                        include_index_state,
                        include_doctor,
                        verify,
                        json,
                    },
            },
        ) => run_sandbox_bundle(&BundleSandboxOptions {
            target: sandbox,
            output: output.as_deref(),
            include: BundleIncludeOptions {
                env: *include_env,
                index_state: *include_index_state,
                doctor: *include_doctor,
            },
            verify: *verify,
            json: *json,
        }),
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
            let source_filter = sandbox_source_filter(source_arg);

            run_sandbox_cleanup(
                *dry_run,
                resolved_failed_only,
                Some(resolved_older_than),
                source_filter,
                *json,
            )
        }
        (
            BuiltInHandlerId::SandboxVerifyBundle,
            Command::Sandbox {
                command:
                    SandboxCommand::VerifyBundle {
                        bundle_dir,
                        strict,
                        json,
                    },
            },
        ) => run_sandbox_verify_bundle(bundle_dir, *strict, *json),
        (
            BuiltInHandlerId::SandboxRebuildIndex,
            Command::Sandbox {
                command: SandboxCommand::RebuildIndex { json },
            },
        ) => run_sandbox_rebuild_index(*json),
        (
            BuiltInHandlerId::SandboxClean,
            Command::Sandbox {
                command:
                    SandboxCommand::Clean {
                        dry_run,
                        all_status,
                        older_than,
                        source,
                        json,
                    },
            },
        ) => {
            let source_filter =
                sandbox_source_filter(source.as_ref().copied().unwrap_or(SandboxSourceArg::All));

            let resolved_failed_only = !all_status;
            let resolved_older_than = older_than.unwrap_or(600);
            run_sandbox_cleanup(
                *dry_run,
                resolved_failed_only,
                Some(resolved_older_than),
                source_filter,
                *json,
            )
        }
        (BuiltInHandlerId::DeviceSealBroker, Command::DeviceSealBroker) => {
            run_device_seal_broker(connection_context).await
        }
        (
            BuiltInHandlerId::SlotList,
            Command::Slot {
                command: SlotCommand::List { format },
            }
            | Command::Env {
                command: SlotCommand::List { format },
            },
        ) => super::slot_cli::run_slot_list(*format),
        (
            BuiltInHandlerId::SlotShow,
            Command::Slot {
                command: SlotCommand::Show { name, format },
            }
            | Command::Env {
                command: SlotCommand::Show { name, format },
            },
        ) => super::slot_cli::run_slot_show(name.as_deref(), *format),
        (
            BuiltInHandlerId::SlotPaths,
            Command::Slot {
                command: SlotCommand::Paths { name },
            }
            | Command::Env {
                command: SlotCommand::Paths { name },
            },
        ) => super::slot_cli::run_slot_paths(name.as_deref()),
        (
            BuiltInHandlerId::SlotDoctor,
            Command::Slot {
                command: SlotCommand::Doctor,
            }
            | Command::Env {
                command: SlotCommand::Doctor,
            },
        ) => super::slot_cli::run_slot_doctor(),
        (
            BuiltInHandlerId::SlotInstall,
            Command::Slot {
                command:
                    SlotCommand::Install {
                        name,
                        binary,
                        no_inherit_base,
                        mode,
                        bin_dir,
                        format,
                        dry_run,
                        overwrite,
                        yes,
                    },
            }
            | Command::Env {
                command:
                    SlotCommand::Install {
                        name,
                        binary,
                        no_inherit_base,
                        mode,
                        bin_dir,
                        format,
                        dry_run,
                        overwrite,
                        yes,
                    },
            },
        ) => super::slot_cli::run_slot_install(super::slot_cli::SlotInstallOptions {
            name,
            binary,
            inherit_base: !*no_inherit_base,
            mode: *mode,
            bin_dir: bin_dir.as_deref(),
            format: *format,
            dry_run: *dry_run,
            overwrite: *overwrite,
            yes: *yes,
        }),
        (
            BuiltInHandlerId::SlotUninstall,
            Command::Slot {
                command:
                    SlotCommand::Uninstall {
                        name,
                        purge,
                        bin_dir,
                    },
            }
            | Command::Env {
                command:
                    SlotCommand::Uninstall {
                        name,
                        purge,
                        bin_dir,
                    },
            },
        ) => super::slot_cli::run_slot_uninstall(name, *purge, bin_dir.as_deref()),
        (
            BuiltInHandlerId::SlotShell,
            Command::Slot {
                command: SlotCommand::Shell { shell },
            }
            | Command::Env {
                command: SlotCommand::Shell { shell },
            },
        ) => super::slot_cli::run_slot_shell(*shell),
        (
            BuiltInHandlerId::SlotExec,
            Command::Slot {
                command: SlotCommand::Exec { slot, argv },
            }
            | Command::Env {
                command: SlotCommand::Exec { slot, argv },
            },
        ) => super::slot_cli::run_slot_exec(slot, argv),
        (
            BuiltInHandlerId::SlotPrint,
            Command::Slot {
                command: SlotCommand::Print { format },
            }
            | Command::Env {
                command: SlotCommand::Print { format },
            },
        ) => super::slot_cli::run_slot_print(*format),
        _ => unreachable!("built-in command handler and command variant should stay in sync"),
    }
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

const fn sandbox_source_filter(source: SandboxSourceArg) -> Option<&'static str> {
    match source {
        SandboxSourceArg::SandboxCli => Some("sandbox-cli"),
        SandboxSourceArg::Playbook => Some("playbook"),
        SandboxSourceArg::RecordingVerify => Some("recording-verify"),
        SandboxSourceArg::All => None,
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
