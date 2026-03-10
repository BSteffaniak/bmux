use crate::cli::{
    Cli, Command, KeymapCommand, PluginCommand, ServerCommand, SessionCommand, TerminalCommand,
    TraceFamily,
};
use crate::connection::{
    ConnectionPolicyScope, ServerRuntimeMetadata, connect, connect_raw, current_cli_build_id,
    map_client_connect_error, read_server_runtime_metadata, remove_server_runtime_metadata_file,
    write_server_runtime_metadata,
};
use crate::input::{InputProcessor, Keymap, RuntimeAction};
use crate::status::{AttachTab, build_attach_status_line};
use anyhow::{Context, Result};
use bmux_client::{AttachLayoutState, AttachSnapshotState, BmuxClient, ClientError};
use bmux_clipboard::ClipboardError;
use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout, TerminfoAutoInstall};
use bmux_ipc::{
    AttachViewComponent, PaneFocusDirection, PaneSplitDirection, SessionRole, SessionSelector,
    SessionSummary, WindowSelector,
};
use bmux_keybind::action_to_name;
use bmux_plugin::{
    CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostMetadata, HostScope,
    NativeCommandContext, NativeLifecycleContext, PluginEvent, PluginEventKind, PluginManifest,
    PluginRegistry, load_registered_plugin as load_native_registered_plugin,
};
use bmux_server::BmuxServer;
use clap::{CommandFactory, FromArgMatches};
use crossterm::cursor::{MoveTo, SavePosition, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::terminal::{Clear, ClearType};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};
use tracing::warn;
use uuid::Uuid;

mod attach;
mod built_in_commands;
mod plugin_commands;
mod plugin_host;
mod terminal_protocol;
use attach::cursor::apply_attach_cursor_state;
use attach::events::{AttachLoopControl, AttachLoopEvent, collect_attach_loop_events};
use attach::render::{
    AttachLayer, AttachLayerSurface, append_pane_output, opaque_row_text, queue_layer_fill,
    render_attach_scene, visible_scene_pane_ids,
};
use attach::state::{
    AttachEventAction, AttachExitReason, AttachScrollbackCursor, AttachScrollbackPosition,
    AttachUiMode, AttachViewState, PaneRect,
};
use built_in_commands::{BuiltInHandlerId, built_in_command_by_handler};
use plugin_commands::PluginCommandRegistry;
use terminal_protocol::{
    ProtocolDirection, ProtocolProfile, ProtocolTraceEvent, primary_da_for_profile,
    protocol_profile_name, secondary_da_for_profile, supported_query_names,
};

const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(200);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_millis(5000);
const ATTACH_IO_POLL_INTERVAL: Duration = Duration::from_millis(15);
const ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE: usize = 1_048_576;
const ATTACH_WINDOW_MODE_UNBOUND_STATUS: &str = "window mode: unbound key (Esc/Enter to exit)";
const ATTACH_SCROLLBACK_UNAVAILABLE_STATUS: &str = "scrollback unavailable for focused pane";
const ATTACH_SELECTION_STARTED_STATUS: &str = "selection started";
const ATTACH_SELECTION_CLEARED_STATUS: &str = "selection cleared";
const ATTACH_SELECTION_COPIED_STATUS: &str = "selection copied";
const ATTACH_SELECTION_EMPTY_STATUS: &str = "no selection";
const ATTACH_TRANSIENT_STATUS_TTL: Duration = Duration::from_millis(1800);
const ATTACH_WELCOME_STATUS_TTL: Duration = Duration::from_millis(2600);
const HELP_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(1);
fn core_provided_capabilities() -> Vec<HostScope> {
    [
        "bmux.commands",
        "bmux.events.subscribe",
        "bmux.key_actions",
        "bmux.status_bar_items",
        "bmux.storage",
        "bmux.clipboard",
        "bmux.sessions.read",
        "bmux.sessions.write",
        "bmux.panes.read",
        "bmux.panes.write",
        "bmux.follow.read",
        "bmux.follow.write",
        "bmux.persistence.read",
        "bmux.persistence.write",
        "bmux.attach.overlay",
        "bmux.terminal.observe",
        "bmux.terminal.input_intercept",
        "bmux.terminal.output_intercept",
    ]
    .into_iter()
    .map(|scope| HostScope::new(scope).expect("supported plugin host scope should parse"))
    .collect()
}

fn available_capability_providers(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<std::collections::BTreeMap<HostScope, bmux_plugin::CapabilityProvider>> {
    registry
        .capability_providers_for(&config.plugins.enabled, &core_provided_capabilities())
        .context("failed resolving capability providers")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProfile {
    Bmux256Color,
    Screen256Color,
    Xterm256Color,
    Conservative,
}

pub async fn run() -> Result<u8> {
    match parse_runtime_cli()? {
        ParsedRuntimeCli::BuiltIn(cli) => {
            init_logging(cli.verbose);

            if let Some(command) = &cli.command {
                return run_command(command).await;
            }

            run_default_server_attach().await
        }
        ParsedRuntimeCli::Plugin {
            verbose,
            plugin_id,
            command_name,
            arguments,
        } => {
            init_logging(verbose);
            run_plugin_command(&plugin_id, &command_name, &arguments).await
        }
        ParsedRuntimeCli::ImmediateExit {
            code,
            output,
            stderr,
        } => {
            if stderr {
                eprint!("{output}");
            } else {
                print!("{output}");
            }
            Ok(code)
        }
    }
}

#[derive(Debug)]
enum ParsedRuntimeCli {
    BuiltIn(Cli),
    ImmediateExit {
        code: u8,
        output: String,
        stderr: bool,
    },
    Plugin {
        verbose: bool,
        plugin_id: String,
        command_name: String,
        arguments: Vec<String>,
    },
}

fn parse_runtime_cli() -> Result<ParsedRuntimeCli> {
    let argv = std::env::args_os().collect::<Vec<_>>();
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    parse_runtime_cli_with_registry(&argv, &config, &registry)
}

fn parse_runtime_cli_with_registry(
    argv: &[std::ffi::OsString],
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<ParsedRuntimeCli> {
    let command_registry = PluginCommandRegistry::build(&config, &registry)
        .context("failed building plugin CLI command registry")?;
    let clap_command = command_registry
        .augment_clap_command(Cli::command())
        .context("failed augmenting CLI with plugin commands")?;
    let matches = match clap_command.try_get_matches_from(argv.iter().cloned()) {
        Ok(matches) => matches,
        Err(error) => {
            return Ok(match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    ParsedRuntimeCli::ImmediateExit {
                        code: 0,
                        output: error.to_string(),
                        stderr: false,
                    }
                }
                _ => ParsedRuntimeCli::ImmediateExit {
                    code: 2,
                    output: error.to_string(),
                    stderr: true,
                },
            });
        }
    };
    let verbose = matches.get_flag("verbose");
    let (path, leaf_matches) = plugin_commands::selected_subcommand_path(&matches);
    if let Some(resolved) = command_registry.resolve_exact_path(&path) {
        let arguments =
            PluginCommandRegistry::normalize_arguments_from_matches(&resolved.schema, leaf_matches);
        return Ok(ParsedRuntimeCli::Plugin {
            verbose,
            plugin_id: resolved.plugin_id,
            command_name: resolved.command_name,
            arguments,
        });
    }

    let cli =
        Cli::from_arg_matches(&matches).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(ParsedRuntimeCli::BuiltIn(cli))
}

async fn run_default_server_attach() -> Result<u8> {
    ensure_server_running_for_default_attach().await?;
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-default-attach").await?;
    let target = resolve_default_attach_target(&mut client).await?;
    let target = target.to_string();
    run_session_attach_with_client(client, Some(target.as_str()), None, false).await
}

async fn ensure_server_running_for_default_attach() -> Result<()> {
    if server_is_running().await? {
        return Ok(());
    }

    let _ = run_server_start(true, false).await?;
    if !server_is_running().await? {
        anyhow::bail!("bmux server failed to start for default attach")
    }
    Ok(())
}

async fn resolve_default_attach_target(client: &mut BmuxClient) -> Result<Uuid> {
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if sessions.is_empty() {
        let name = next_default_session_name(&sessions);
        let id = client
            .new_session(Some(name.clone()))
            .await
            .map_err(map_cli_client_error)?;
        return Ok(id);
    }

    let client_id = client.whoami().await.map_err(map_cli_client_error)?;
    let mut writable_sessions = Vec::new();
    for session in &sessions {
        let permissions = client
            .list_permissions(SessionSelector::ById(session.id))
            .await
            .map_err(map_cli_client_error)?;
        let role = permissions
            .iter()
            .find(|permission| permission.client_id == client_id)
            .map_or(SessionRole::Observer, |permission| permission.role);
        if role == SessionRole::Owner || role == SessionRole::Writer {
            writable_sessions.push(session.clone());
        }
    }

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

fn next_default_session_name(sessions: &[SessionSummary]) -> String {
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

async fn run_command(command: &Command) -> Result<u8> {
    match command {
        Command::External(args) => run_external_plugin_command(args).await,
        _ => dispatch_built_in_command(command).await,
    }
}

fn built_in_handler_for_command(command: &Command) -> BuiltInHandlerId {
    match command {
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
        Command::Server { command } => match command {
            ServerCommand::Start { .. } => BuiltInHandlerId::ServerStart,
            ServerCommand::Status { .. } => BuiltInHandlerId::ServerStatus,
            ServerCommand::WhoamiPrincipal { .. } => BuiltInHandlerId::ServerWhoamiPrincipal,
            ServerCommand::Save => BuiltInHandlerId::ServerSave,
            ServerCommand::Restore { .. } => BuiltInHandlerId::ServerRestore,
            ServerCommand::Stop => BuiltInHandlerId::ServerStop,
        },
        Command::Keymap { .. } => BuiltInHandlerId::KeymapDoctor,
        Command::Terminal { command } => match command {
            TerminalCommand::Doctor { .. } => BuiltInHandlerId::TerminalDoctor,
            TerminalCommand::InstallTerminfo { .. } => BuiltInHandlerId::TerminalInstallTerminfo,
        },
        Command::Plugin { command } => match command {
            PluginCommand::List { .. } => BuiltInHandlerId::PluginList,
            PluginCommand::Run { .. } => BuiltInHandlerId::PluginRun,
        },
        Command::External(_) => unreachable!("external commands are dispatched separately"),
    }
}

async fn dispatch_built_in_command(command: &Command) -> Result<u8> {
    let handler = built_in_handler_for_command(command);
    let _descriptor = built_in_command_by_handler(handler);
    match (handler, command) {
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
            BuiltInHandlerId::PluginList,
            Command::Plugin {
                command: PluginCommand::List { json },
            },
        ) => run_plugin_list(*json).await,
        (
            BuiltInHandlerId::PluginRun,
            Command::Plugin {
                command:
                    PluginCommand::Run {
                        plugin,
                        command,
                        args,
                    },
            },
        ) => run_plugin_command(plugin, command, args).await,
        _ => unreachable!("built-in command handler and command variant should stay in sync"),
    }
}

async fn run_server_start(daemon: bool, foreground_internal: bool) -> Result<u8> {
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
        child
            .arg("server")
            .arg("start")
            .arg("--foreground-internal")
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
    let server = BmuxServer::from_default_paths();
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

fn plugin_host_metadata() -> HostMetadata {
    HostMetadata {
        product_name: "bmux".to_string(),
        product_version: env!("CARGO_PKG_VERSION").to_string(),
        plugin_api_version: CURRENT_PLUGIN_API_VERSION,
        plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
    }
}

fn plugin_host_for_declaration(
    declaration: &bmux_plugin::PluginDeclaration,
    paths: &ConfigPaths,
    config: &BmuxConfig,
) -> plugin_host::CliPluginHost {
    plugin_host::CliPluginHost::for_plugin(
        declaration.id.as_str(),
        plugin_host_metadata(),
        paths,
        config.clone(),
        declaration.required_capabilities.clone(),
        declaration.provided_capabilities.clone(),
    )
}

#[cfg(test)]
fn validate_configured_plugins(config: &BmuxConfig, paths: &ConfigPaths) -> Result<()> {
    let registry = scan_available_plugins(config, paths)?;
    validate_enabled_plugins(config, &registry)
}

fn scan_available_plugins(config: &BmuxConfig, paths: &ConfigPaths) -> Result<PluginRegistry> {
    let search_paths = resolve_plugin_search_paths(config, paths)?;
    let reports = bmux_plugin::discover_plugin_manifests_in_roots(&search_paths)?;
    let mut registry = PluginRegistry::new();
    for report in reports {
        for manifest_path in report.manifest_paths {
            match PluginManifest::from_path(&manifest_path) {
                Ok(manifest) => {
                    if let Err(error) = registry.register_manifest_from_root(
                        &report.search_root,
                        &manifest_path,
                        manifest,
                    ) {
                        warn!(
                            "skipping plugin manifest {} during enabled-plugin scan: {error}",
                            manifest_path.display()
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        "skipping unreadable plugin manifest {} during enabled-plugin scan: {error}",
                        manifest_path.display()
                    );
                }
            }
        }
    }
    Ok(registry)
}

fn resolve_plugin_search_paths(config: &BmuxConfig, paths: &ConfigPaths) -> Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            let shipped = parent.join("plugins");
            if seen.insert(shipped.clone()) {
                resolved.push(shipped);
            }
        }
    }

    let user_plugins = paths.plugins_dir();
    if seen.insert(user_plugins.clone()) {
        resolved.push(user_plugins);
    }

    for search_path in &config.plugins.search_paths {
        let absolute = if search_path.is_absolute() {
            search_path.clone()
        } else {
            std::env::current_dir()
                .context("failed resolving current directory for plugin search path")?
                .join(search_path)
        };
        if seen.insert(absolute.clone()) {
            resolved.push(absolute);
        }
    }

    Ok(resolved)
}

fn validate_enabled_plugins(config: &BmuxConfig, registry: &PluginRegistry) -> Result<()> {
    if config.plugins.enabled.is_empty() {
        return Ok(());
    }

    for plugin_id in &config.plugins.enabled {
        let _ = registry.get(plugin_id).with_context(|| {
            let available = registry.plugin_ids();
            if available.is_empty() {
                format!(
                    "enabled plugin '{plugin_id}' was not found in the configured plugins directory"
                )
            } else {
                format!(
                    "enabled plugin '{plugin_id}' was not found in the configured plugins directory (available: {})",
                    available.join(", ")
                )
            }
        })?;
    }

    let host = plugin_host_metadata();
    let available_capabilities = available_capability_providers(config, registry)?;
    let ordered_plugins = registry
        .activation_order_for(&config.plugins.enabled)
        .context("enabled plugin dependency graph is invalid")?;
    for plugin in ordered_plugins {
        let plugin_id = plugin.declaration.id.as_str();
        bmux_plugin::PluginRegistry::validate_registered_plugin(
            plugin,
            &host,
            &available_capabilities,
        )
        .with_context(|| format!("failed validating enabled plugin '{plugin_id}'"))?;
    }

    PluginCommandRegistry::build(config, registry)
        .context("failed building plugin CLI command registry")?;

    Ok(())
}

fn load_enabled_plugins(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<Vec<bmux_plugin::LoadedPlugin>> {
    if config.plugins.enabled.is_empty() {
        return Ok(Vec::new());
    }

    for plugin_id in &config.plugins.enabled {
        let _ = registry.get(plugin_id).with_context(|| {
            format!("enabled plugin '{plugin_id}' disappeared during native load")
        })?;
    }

    let host = plugin_host_metadata();
    let available_capabilities = available_capability_providers(config, registry)?;
    let ordered_plugins = registry
        .activation_order_for(&config.plugins.enabled)
        .context("enabled plugin dependency graph is invalid")?;
    let mut loaded_plugins = Vec::with_capacity(ordered_plugins.len());
    for plugin in ordered_plugins {
        let plugin_id = plugin.declaration.id.as_str();
        let loaded = load_native_registered_plugin(plugin, &host, &available_capabilities)
            .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"))?;
        loaded_plugins.push(loaded);
    }

    Ok(loaded_plugins)
}

fn plugin_lifecycle_context(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    declaration: &bmux_plugin::PluginDeclaration,
) -> NativeLifecycleContext {
    let host = plugin_host_for_declaration(declaration, paths, config);
    NativeLifecycleContext {
        plugin_id: declaration.id.as_str().to_string(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        host: plugin_host_metadata(),
        connection: bmux_plugin::PluginHost::connection(&host).clone(),
        settings: config
            .plugins
            .settings
            .get(declaration.id.as_str())
            .cloned(),
    }
}

fn plugin_command_context(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    declaration: &bmux_plugin::PluginDeclaration,
    command: &str,
    arguments: &[String],
) -> NativeCommandContext {
    let host = plugin_host_for_declaration(declaration, paths, config);
    NativeCommandContext {
        plugin_id: declaration.id.as_str().to_string(),
        command: command.to_string(),
        arguments: arguments.to_vec(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        host: plugin_host_metadata(),
        connection: bmux_plugin::PluginHost::connection(&host).clone(),
        settings: config
            .plugins
            .settings
            .get(declaration.id.as_str())
            .cloned(),
    }
}

fn plugin_system_event(name: &str) -> PluginEvent {
    PluginEvent {
        kind: PluginEventKind::System,
        name: name.to_string(),
        payload: serde_json::json!({
            "product": "bmux",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    }
}

fn activate_loaded_plugins(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    let mut activated: Vec<&bmux_plugin::LoadedPlugin> = Vec::new();
    for plugin in loaded_plugins {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let context = plugin_lifecycle_context(config, paths, &plugin.declaration);
        if let Err(error) = plugin.activate(&context) {
            for activated_plugin in activated.into_iter().rev() {
                let context =
                    plugin_lifecycle_context(config, paths, &activated_plugin.declaration);
                if let Err(deactivate_error) = activated_plugin.deactivate(&context) {
                    warn!(
                        "failed rolling back plugin activation for {}: {deactivate_error}",
                        activated_plugin.declaration.id.as_str()
                    );
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "failed activating plugin '{}'",
                    plugin.declaration.id.as_str()
                )
            });
        }

        activated.push(plugin);
    }

    Ok(())
}

fn deactivate_loaded_plugins(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    for plugin in loaded_plugins.iter().rev() {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let context = plugin_lifecycle_context(config, paths, &plugin.declaration);
        let _ = plugin.deactivate(&context).with_context(|| {
            format!(
                "failed deactivating plugin '{}'",
                plugin.declaration.id.as_str()
            )
        })?;
    }

    Ok(())
}

fn dispatch_loaded_plugin_event(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    event: PluginEvent,
) -> Result<()> {
    for plugin in loaded_plugins {
        let _ = plugin.dispatch_event(&event).with_context(|| {
            format!(
                "failed dispatching plugin event '{}' to '{}'",
                event.name,
                plugin.declaration.id.as_str()
            )
        })?;
    }

    Ok(())
}

async fn plugin_event_bridge_loop(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if loaded_plugins.is_empty() {
        return Ok(());
    }

    let mut client = loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        match connect_raw("bmux-plugin-event-bridge").await {
            Ok(client) => break client,
            Err(_) => {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return Ok(());
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        }
    };

    client
        .subscribe_events()
        .await
        .map_err(map_cli_client_error)?;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            result = client.poll_events(32) => {
                let events = result.map_err(map_cli_client_error)?;
                for event in events {
                    dispatch_loaded_plugin_event(loaded_plugins, plugin_event_from_server_event(&event)?)?;
                }
            }
        }
    }
}

fn plugin_event_from_server_event(event: &bmux_client::ServerEvent) -> Result<PluginEvent> {
    Ok(PluginEvent {
        kind: plugin_event_kind_from_server_event(event),
        name: server_event_name(event).to_string(),
        payload: serde_json::to_value(event).context("failed encoding server event payload")?,
    })
}

const fn plugin_event_kind_from_server_event(event: &bmux_client::ServerEvent) -> PluginEventKind {
    match event {
        bmux_client::ServerEvent::ServerStarted | bmux_client::ServerEvent::ServerStopping => {
            PluginEventKind::System
        }
        bmux_client::ServerEvent::SessionCreated { .. }
        | bmux_client::ServerEvent::SessionRemoved { .. }
        | bmux_client::ServerEvent::FollowStarted { .. }
        | bmux_client::ServerEvent::FollowStopped { .. }
        | bmux_client::ServerEvent::FollowTargetGone { .. }
        | bmux_client::ServerEvent::FollowTargetChanged { .. }
        | bmux_client::ServerEvent::RoleChanged { .. } => PluginEventKind::Session,
        bmux_client::ServerEvent::WindowCreated { .. }
        | bmux_client::ServerEvent::WindowRemoved { .. }
        | bmux_client::ServerEvent::WindowSwitched { .. } => PluginEventKind::Window,
        bmux_client::ServerEvent::ClientAttached { .. }
        | bmux_client::ServerEvent::ClientDetached { .. } => PluginEventKind::Client,
        bmux_client::ServerEvent::AttachViewChanged { .. } => PluginEventKind::Pane,
    }
}

#[derive(Debug, serde::Serialize)]
struct PluginListJsonEntry {
    id: String,
    display_name: String,
    version: String,
    enabled: bool,
    required_capabilities: Vec<String>,
    provided_capabilities: Vec<String>,
    commands: Vec<String>,
}

async fn run_plugin_list(as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let enabled = config
        .plugins
        .enabled
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut entries = registry
        .iter()
        .map(|plugin| PluginListJsonEntry {
            id: plugin.declaration.id.as_str().to_string(),
            display_name: plugin.declaration.display_name.clone(),
            version: plugin.declaration.plugin_version.clone(),
            enabled: enabled.contains(&plugin.declaration.id.as_str().to_string()),
            required_capabilities: plugin
                .declaration
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            provided_capabilities: plugin
                .declaration
                .provided_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            commands: plugin
                .declaration
                .commands
                .iter()
                .map(|command| command.name.clone())
                .collect(),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.id.cmp(&right.id));

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).context("failed encoding plugin list json")?
        );
    } else if entries.is_empty() {
        println!("no plugins discovered");
    } else {
        for entry in entries {
            println!(
                "{}{} - {} ({})",
                entry.id,
                if entry.enabled { " [enabled]" } else { "" },
                entry.display_name,
                entry.version
            );
            if !entry.commands.is_empty() {
                println!("  commands: {}", entry.commands.join(", "));
            }
            if !entry.required_capabilities.is_empty() {
                println!(
                    "  required capabilities: {}",
                    entry.required_capabilities.join(", ")
                );
            }
            if !entry.provided_capabilities.is_empty() {
                println!(
                    "  provided capabilities: {}",
                    entry.provided_capabilities.join(", ")
                );
            }
        }
    }

    Ok(0)
}

async fn run_plugin_command(plugin_id: &str, command_name: &str, args: &[String]) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let plugin = registry.get(plugin_id).with_context(|| {
        let available = registry.plugin_ids();
        if available.is_empty() {
            format!("plugin '{plugin_id}' was not found")
        } else {
            format!(
                "plugin '{plugin_id}' was not found (available: {})",
                available.join(", ")
            )
        }
    })?;

    if !config
        .plugins
        .enabled
        .iter()
        .any(|enabled| enabled == plugin_id)
    {
        anyhow::bail!(
            "plugin '{plugin_id}' is not enabled in config; add it under plugins.enabled to run commands"
        );
    }

    let loaded = load_native_registered_plugin(
        plugin,
        &plugin_host_metadata(),
        &available_capability_providers(&config, &registry)?,
    )
    .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"))?;
    let context = plugin_command_context(&config, &paths, &plugin.declaration, command_name, args);
    let status = loaded
        .run_command_with_context(command_name, args, Some(&context))
        .with_context(|| format!("failed running plugin command '{plugin_id}:{command_name}'"))?;
    Ok(status.clamp(0, i32::from(u8::MAX)) as u8)
}

async fn run_external_plugin_command(args: &[String]) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let command_registry = PluginCommandRegistry::build(&config, &registry)
        .context("failed building plugin CLI command registry")?;
    let resolved = command_registry.resolve(args).with_context(|| {
        format!(
            "unknown command '{}'; run 'bmux plugin list' to inspect available plugins",
            args.join(" ")
        )
    })?;
    let validated_arguments =
        PluginCommandRegistry::validate_arguments(&resolved.schema, &resolved.arguments)
            .context("failed validating plugin command arguments")?;
    run_plugin_command(
        &resolved.plugin_id,
        &resolved.command_name,
        &validated_arguments,
    )
    .await
}

#[derive(Debug, serde::Serialize)]
struct ServerStatusJsonPayload {
    running: bool,
    principal_id: Option<Uuid>,
    server_owner_principal_id: Option<Uuid>,
    force_local_authorized: bool,
    latest_server_event: Option<String>,
    snapshot: Option<bmux_ipc::ServerSnapshotStatus>,
    server_metadata: Option<ServerRuntimeMetadata>,
    cli_build: Option<String>,
    stale_build: bool,
    stale_warning: Option<String>,
}

async fn run_server_status(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let status = fetch_server_status().await?;
    let metadata = read_server_runtime_metadata()?;
    let current_build_id = current_cli_build_id().ok();
    let stale_warning = metadata.as_ref().and_then(|entry| {
        current_build_id
            .as_ref()
            .filter(|build| entry.build_id != **build)
            .map(|build| {
                format!(
                    "running server build ({}) differs from current CLI build ({}); restart with `bmux server stop`",
                    entry.build_id, build
                )
            })
    });
    let stale_build = stale_warning.is_some();

    if as_json {
        let latest_event = if matches!(status, Some(ref s) if s.running) {
            latest_server_event_name().await?.map(str::to_string)
        } else {
            None
        };
        let payload = ServerStatusJsonPayload {
            running: matches!(status, Some(ref s) if s.running),
            principal_id: status.as_ref().map(|entry| entry.principal_id),
            server_owner_principal_id: status.as_ref().map(|entry| entry.server_owner_principal_id),
            force_local_authorized: status
                .as_ref()
                .is_some_and(|entry| entry.principal_id == entry.server_owner_principal_id),
            latest_server_event: latest_event,
            snapshot: status.as_ref().map(|entry| entry.snapshot.clone()),
            server_metadata: metadata,
            cli_build: current_build_id,
            stale_build,
            stale_warning,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).context("failed encoding server status json")?
        );
        return Ok(u8::from(!payload.running));
    }

    match status {
        Some(status) if status.running => {
            if let Some(event_name) = latest_server_event_name().await? {
                println!("latest server event: {event_name}");
            }
            if let Some(metadata) = metadata.as_ref() {
                println!("server pid: {}", metadata.pid);
                println!("server version: {}", metadata.version);
                println!("server build: {}", metadata.build_id);
                println!("server executable: {}", metadata.executable_path);
                println!("server started_at_ms: {}", metadata.started_at_epoch_ms);
            } else {
                println!("server metadata: missing");
            }
            if let Some(build_id) = current_build_id.as_ref() {
                println!("cli build: {build_id}");
                if let Some(warning) = stale_warning.as_ref() {
                    println!("warning: {warning}");
                }
            }
            println!("principal id: {}", status.principal_id);
            println!(
                "server owner principal id: {}",
                status.server_owner_principal_id
            );
            println!(
                "force-local authorized: {}",
                if status.principal_id == status.server_owner_principal_id {
                    "yes"
                } else {
                    "no"
                }
            );
            println!(
                "snapshot: {}{}",
                if status.snapshot.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status
                    .snapshot
                    .path
                    .as_ref()
                    .map_or(String::new(), |path| format!(" ({path})"))
            );
            if status.snapshot.enabled {
                println!(
                    "snapshot file: {}",
                    if status.snapshot.snapshot_exists {
                        "present"
                    } else {
                        "missing"
                    }
                );
                if let Some(last_write) = status.snapshot.last_write_epoch_ms {
                    println!("snapshot last write (ms): {last_write}");
                }
                if let Some(last_restore) = status.snapshot.last_restore_epoch_ms {
                    println!("snapshot last restore (ms): {last_restore}");
                }
                if let Some(error) = status.snapshot.last_restore_error.as_ref() {
                    println!("snapshot last error: {error}");
                }
            }
            println!("bmux server is running");
            Ok(0)
        }
        _ => {
            println!("bmux server is not running");
            Ok(1)
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct ServerWhoAmIPrincipalJsonPayload {
    principal_id: Uuid,
    server_owner_principal_id: Uuid,
    force_local_authorized: bool,
}

async fn run_server_whoami_principal(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_raw("bmux-cli-server-whoami-principal").await?;
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;

    if as_json {
        let payload = ServerWhoAmIPrincipalJsonPayload {
            principal_id: identity.principal_id,
            server_owner_principal_id: identity.server_owner_principal_id,
            force_local_authorized: identity.force_local_authorized,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding server whoami-principal json")?
        );
        return Ok(0);
    }

    println!("principal id: {}", identity.principal_id);
    println!(
        "server owner principal id: {}",
        identity.server_owner_principal_id
    );
    println!(
        "force-local authorized: {}",
        if identity.force_local_authorized {
            "yes"
        } else {
            "no"
        }
    );
    Ok(0)
}

async fn run_server_save() -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-server-save").await?;
    let path = client.server_save().await.map_err(map_cli_client_error)?;

    match path {
        Some(path) => println!("snapshot saved: {path}"),
        None => println!("snapshot save requested"),
    }
    Ok(0)
}

async fn run_server_restore(dry_run: bool, yes: bool) -> Result<u8> {
    if !dry_run && !yes {
        anyhow::bail!("server restore requires either --dry-run or --yes");
    }
    cleanup_stale_pid_file().await?;

    if dry_run {
        let mut client = connect(
            ConnectionPolicyScope::Normal,
            "bmux-cli-server-restore-dry-run",
        )
        .await?;
        let (ok, message) = client
            .server_restore_dry_run()
            .await
            .map_err(map_cli_client_error)?;

        if ok {
            println!("restore dry-run: OK - {message}");
            return Ok(0);
        }
        println!("restore dry-run: FAIL - {message}");
        return Ok(1);
    }

    let mut client = connect(
        ConnectionPolicyScope::Normal,
        "bmux-cli-server-restore-apply",
    )
    .await?;
    let summary = client
        .server_restore_apply()
        .await
        .map_err(map_cli_client_error)?;

    println!(
        "restore applied: sessions={}, windows={}, roles={}, follows={}, selected_sessions={}",
        summary.sessions,
        summary.windows,
        summary.roles,
        summary.follows,
        summary.selected_sessions
    );
    Ok(0)
}

async fn latest_server_event_name() -> Result<Option<&'static str>> {
    let connect =
        tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-status-events")).await;

    let mut client = match connect {
        Ok(Ok(client)) => client,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    let _ = tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.subscribe_events()).await;
    let events = match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.poll_events(1)).await {
        Ok(Ok(events)) => events,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };
    Ok(events.last().map(server_event_name))
}

const fn server_event_name(event: &bmux_client::ServerEvent) -> &'static str {
    match event {
        bmux_client::ServerEvent::ServerStarted => "server_started",
        bmux_client::ServerEvent::ServerStopping => "server_stopping",
        bmux_client::ServerEvent::SessionCreated { .. } => "session_created",
        bmux_client::ServerEvent::SessionRemoved { .. } => "session_removed",
        bmux_client::ServerEvent::WindowCreated { .. } => "window_created",
        bmux_client::ServerEvent::WindowRemoved { .. } => "window_removed",
        bmux_client::ServerEvent::WindowSwitched { .. } => "window_switched",
        bmux_client::ServerEvent::ClientAttached { .. } => "client_attached",
        bmux_client::ServerEvent::ClientDetached { .. } => "client_detached",
        bmux_client::ServerEvent::FollowStarted { .. } => "follow_started",
        bmux_client::ServerEvent::FollowStopped { .. } => "follow_stopped",
        bmux_client::ServerEvent::FollowTargetGone { .. } => "follow_target_gone",
        bmux_client::ServerEvent::FollowTargetChanged { .. } => "follow_target_changed",
        bmux_client::ServerEvent::RoleChanged { .. } => "role_changed",
        bmux_client::ServerEvent::AttachViewChanged { .. } => "attach_view_changed",
    }
}

async fn run_server_stop() -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let graceful_stopped =
        match tokio::time::timeout(SERVER_STOP_TIMEOUT, connect_raw("bmux-cli-stop")).await {
            Ok(Ok(mut client)) => {
                client.stop_server().await.map_err(map_cli_client_error)?;
                wait_until_server_stopped(SERVER_STOP_TIMEOUT).await?
            }
            Ok(Err(_)) | Err(_) => false,
        };

    if graceful_stopped {
        println!("bmux server stopped gracefully");
        let _ = remove_server_pid_file();
        return Ok(0);
    }

    if let Some(pid) = read_server_pid_file()? {
        if try_kill_pid(pid)? {
            if wait_for_process_exit(pid, SERVER_STOP_TIMEOUT)? {
                println!("bmux server stop fallback succeeded (pid {pid})");
                let _ = remove_server_pid_file();
                return Ok(0);
            }
        } else if !is_pid_running(pid)? {
            let _ = remove_server_pid_file();
        }
    }

    println!("bmux server is not running");
    Ok(1)
}

async fn run_session_new(name: Option<String>) -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-new-session").await?;
    let session_id = client
        .new_session(name)
        .await
        .map_err(map_cli_client_error)?;
    println!("created session: {session_id}");
    Ok(0)
}

async fn run_session_list(as_json: bool) -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-list-sessions").await?;
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&sessions).context("failed to encode sessions json")?
        );
        return Ok(0);
    }

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }

    println!("ID                                   NAME            WINDOWS CLIENTS");
    for session in sessions {
        let name = session.name.unwrap_or_else(|| "-".to_string());
        println!(
            "{:<36} {:<15} {:<7} {}",
            session.id, name, session.window_count, session.client_count
        );
    }

    Ok(0)
}

async fn run_client_list(as_json: bool) -> Result<u8> {
    let mut api = connect(ConnectionPolicyScope::Normal, "bmux-cli-list-clients").await?;
    let self_id = api.whoami().await.map_err(map_cli_client_error)?;
    let clients = api.list_clients().await.map_err(map_cli_client_error)?;
    let mut clients = clients;
    clients.sort_by_key(|client| (client.id != self_id, client.id));

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&clients).context("failed to encode clients json")?
        );
        return Ok(0);
    }

    if clients.is_empty() {
        println!("no clients");
        return Ok(0);
    }

    let sessions = api.list_sessions().await.map_err(map_cli_client_error)?;
    println!(
        "ID                                   SELF ROLE      SESSION          WINDOW       FOLLOWING_CLIENT                     GLOBAL"
    );
    for client_summary in clients {
        let role = client_summary.session_role.map_or("-", session_role_label);
        let selected_session = client_summary.selected_session_id.map_or_else(
            || "-".to_string(),
            |id| {
                sessions
                    .iter()
                    .find(|session| session.id == id)
                    .map(session_summary_label)
                    .unwrap_or_else(|| format!("session-{}", short_uuid(id)))
            },
        );
        let selected_window = if let Some(session_id) = client_summary.selected_session_id {
            let mut windows = api
                .list_windows(Some(SessionSelector::ById(session_id)))
                .await
                .map_err(map_cli_client_error)?;
            sort_attach_windows(&mut windows);
            windows
                .iter()
                .find(|window| window.active)
                .map(window_summary_label)
                .unwrap_or_else(|| "-".to_string())
        } else {
            "-".to_string()
        };
        let following_client = client_summary
            .following_client_id
            .map_or_else(|| "-".to_string(), |id| id.to_string());
        println!(
            "{:<36} {:<4} {:<9} {:<16} {:<12} {:<36} {}",
            client_summary.id,
            if client_summary.id == self_id {
                "yes"
            } else {
                "no"
            },
            role,
            selected_session,
            selected_window,
            following_client,
            if client_summary.following_global {
                "yes"
            } else {
                "no"
            }
        );
    }

    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestructiveOpErrorKind {
    OwnerRoleRequired,
    ForceLocalUnauthorized,
    NotFound,
    Other,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct KillFailureSummary {
    permission_denied: usize,
    not_found: usize,
    other: usize,
}

impl KillFailureSummary {
    const fn record(&mut self, kind: DestructiveOpErrorKind) {
        match kind {
            DestructiveOpErrorKind::OwnerRoleRequired
            | DestructiveOpErrorKind::ForceLocalUnauthorized => {
                self.permission_denied = self.permission_denied.saturating_add(1);
            }
            DestructiveOpErrorKind::NotFound => {
                self.not_found = self.not_found.saturating_add(1);
            }
            DestructiveOpErrorKind::Other => {
                self.other = self.other.saturating_add(1);
            }
        }
    }
}

fn classify_destructive_op_error(error: &ClientError) -> DestructiveOpErrorKind {
    match error {
        ClientError::ServerError { code, message } => match code {
            bmux_ipc::ErrorCode::InvalidRequest
                if message.contains("owner role required for this operation") =>
            {
                DestructiveOpErrorKind::OwnerRoleRequired
            }
            bmux_ipc::ErrorCode::InvalidRequest
                if message
                    .contains("force-local is only allowed for the server owner principal") =>
            {
                DestructiveOpErrorKind::ForceLocalUnauthorized
            }
            bmux_ipc::ErrorCode::NotFound => DestructiveOpErrorKind::NotFound,
            _ => DestructiveOpErrorKind::Other,
        },
        _ => DestructiveOpErrorKind::Other,
    }
}

fn format_destructive_op_error(noun: &str, error: ClientError, force_local: bool) -> String {
    match classify_destructive_op_error(&error) {
        DestructiveOpErrorKind::OwnerRoleRequired => format!(
            "{noun} kill requires the session owner role. Inspect roles with `bmux session permissions <session>`.{}",
            if force_local {
                " If you intended to override locally, use `--force-local` only from the server owner principal."
            } else {
                ""
            }
        ),
        DestructiveOpErrorKind::ForceLocalUnauthorized =>
            "`--force-local` is only available to the server owner principal. Check `bmux server whoami-principal`."
                .to_string(),
        DestructiveOpErrorKind::NotFound => map_cli_client_error(error).to_string(),
        DestructiveOpErrorKind::Other => map_cli_client_error(error).to_string(),
    }
}

async fn kill_preflight_identity(
    client: &mut BmuxClient,
    force_local: bool,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    if !force_local {
        return Ok(None);
    }
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;
    if !identity.force_local_authorized {
        anyhow::bail!(
            "`--force-local` is only available to the server owner principal.\ncurrent principal: {}\nserver owner principal: {}\nInspect with `bmux server whoami-principal`.",
            identity.principal_id,
            identity.server_owner_principal_id
        );
    }
    Ok(Some(identity))
}

async fn print_bulk_kill_preflight(
    client: &mut BmuxClient,
    noun: &str,
    force_local: bool,
) -> Result<Option<bmux_client::PrincipalIdentityInfo>> {
    let identity = client
        .whoami_principal()
        .await
        .map_err(map_cli_client_error)?;
    if force_local {
        if !identity.force_local_authorized {
            anyhow::bail!(
                "`--force-local` is only available to the server owner principal.\ncurrent principal: {}\nserver owner principal: {}\nInspect with `bmux server whoami-principal`.",
                identity.principal_id,
                identity.server_owner_principal_id
            );
        }
        println!(
            "kill-all {noun}: force-local enabled for principal {}",
            identity.principal_id
        );
        return Ok(Some(identity));
    }

    println!(
        "kill-all {noun}: principal {} (server owner: {})",
        identity.principal_id, identity.server_owner_principal_id
    );
    println!(
        "note: non-owned {noun} may fail; inspect roles with `bmux session permissions <session>`"
    );
    Ok(Some(identity))
}

fn print_bulk_kill_failure_summary(noun: &str, summary: KillFailureSummary) {
    if summary == KillFailureSummary::default() {
        return;
    }
    println!(
        "{noun} kill failures: permission_denied={}, not_found={}, other={}",
        summary.permission_denied, summary.not_found, summary.other
    );
    if summary.permission_denied > 0 {
        println!(
            "hint: inspect roles with `bmux session permissions <session>` or identity with `bmux server whoami-principal`"
        );
    }
}

fn attach_quit_failure_status(error: &ClientError) -> &'static str {
    match classify_destructive_op_error(error) {
        DestructiveOpErrorKind::OwnerRoleRequired => "quit requires session owner",
        DestructiveOpErrorKind::ForceLocalUnauthorized => {
            "quit requires server owner for --force-local"
        }
        DestructiveOpErrorKind::NotFound => "quit failed: session not found",
        DestructiveOpErrorKind::Other => "quit failed",
    }
}

async fn run_session_kill(target: &str, force_local: bool) -> Result<u8> {
    let selector = parse_session_selector(target);
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-kill-session").await?;
    let _ = kill_preflight_identity(&mut client, force_local).await?;
    let killed_id = client
        .kill_session_with_options(selector, force_local)
        .await
        .map_err(|error| {
            anyhow::anyhow!(format_destructive_op_error("session", error, force_local))
        })?;
    println!("killed session: {killed_id}");
    Ok(0)
}

async fn run_session_kill_all(force_local: bool) -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-kill-all-sessions").await?;
    let _ = print_bulk_kill_preflight(&mut client, "sessions", force_local).await?;
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }

    let mut killed_count = 0usize;
    let mut failed_count = 0usize;
    let mut failure_summary = KillFailureSummary::default();
    for session in sessions {
        match client
            .kill_session_with_options(SessionSelector::ById(session.id), force_local)
            .await
        {
            Ok(killed_id) => {
                println!("killed session: {killed_id}");
                killed_count = killed_count.saturating_add(1);
            }
            Err(error) => {
                failed_count = failed_count.saturating_add(1);
                let kind = classify_destructive_op_error(&error);
                failure_summary.record(kind);
                let mapped_error = format_destructive_op_error("session", error, force_local);
                eprintln!("failed killing session {}: {mapped_error}", session.id);
            }
        }
    }

    println!("kill-all-sessions complete: killed {killed_count}, failed {failed_count}");
    print_bulk_kill_failure_summary("session", failure_summary);
    Ok(u8::from(failed_count != 0))
}

async fn run_session_attach(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    let client = connect(ConnectionPolicyScope::Normal, "bmux-cli-attach").await?;
    run_session_attach_with_client(client, target, follow, global).await
}

async fn run_session_attach_with_client(
    mut client: BmuxClient,
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    if target.is_none() && follow.is_none() {
        anyhow::bail!("attach requires a session target or --follow <client-uuid>");
    }
    if target.is_some() && follow.is_some() {
        anyhow::bail!("attach accepts either a session target or --follow, not both");
    }

    let follow_target_id = match follow {
        Some(follow_target) => Some(parse_uuid_value(follow_target, "follow target client id")?),
        None => None,
    };

    let attach_config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "bmux warning: failed loading config for attach keymap, using defaults ({error})"
            );
            BmuxConfig::default()
        }
    };
    let attach_keymap = attach_keymap_from_config(&attach_config);
    let attach_help_lines = build_attach_help_lines(&attach_config);
    let mut attach_input_processor = InputProcessor::new(attach_keymap.clone());

    if let Some(leader_client_id) = follow_target_id {
        client
            .subscribe_events()
            .await
            .map_err(map_attach_client_error)?;
        client
            .follow_client(leader_client_id, global)
            .await
            .map_err(map_attach_client_error)?;
    }

    let self_client_id = client.whoami().await.map_err(map_attach_client_error)?;

    let attach_info = if let Some(leader_client_id) = follow_target_id {
        let target_session = resolve_follow_target_session(&mut client, leader_client_id)
            .await
            .map_err(map_attach_client_error)?;
        open_attach_for_session(&mut client, target_session)
            .await
            .map_err(map_attach_client_error)?
    } else {
        let target = target.expect("target is present when not follow");
        let grant = client
            .attach_grant(parse_session_selector(target))
            .await
            .map_err(map_attach_client_error)?;
        client
            .open_attach_stream_info(&grant)
            .await
            .map_err(map_attach_client_error)?
    };

    if let Some(leader_client_id) = follow_target_id {
        println!(
            "attached to session: {} (following {}{})",
            attach_info.session_id,
            leader_client_id,
            if global { ", global" } else { "" }
        );
    } else {
        println!("attached to session: {}", attach_info.session_id);
    }

    let mut view_state = AttachViewState::new(attach_info);

    update_attach_viewport(&mut client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(&mut client, &mut view_state).await?;
    view_state.set_transient_status(
        initial_attach_status(&attach_keymap, view_state.can_write),
        Instant::now(),
        ATTACH_WELCOME_STATUS_TTL,
    );

    if !view_state.can_write {
        println!("read-only attach: input disabled");
    }
    if let Some(detach_key) = attach_keymap.primary_binding_for_action(&RuntimeAction::Detach) {
        println!("press {detach_key} to detach");
    } else {
        println!("detach is unbound in current keymap");
    }
    client
        .subscribe_events()
        .await
        .map_err(map_attach_client_error)?;
    let _ = client
        .poll_events(256)
        .await
        .map_err(map_attach_client_error)?;

    let raw_mode_guard = RawModeGuard::enable().context("failed to enable raw mode for attach")?;
    let mut exit_reason = AttachExitReason::Detached;

    loop {
        let server_events = client
            .poll_events(16)
            .await
            .map_err(map_attach_client_error)?;
        let terminal_event = poll_attach_terminal_event(ATTACH_IO_POLL_INTERVAL).await?;
        let loop_events = collect_attach_loop_events(server_events, terminal_event);
        let mut should_break = false;
        for loop_event in loop_events {
            match handle_attach_loop_event(
                loop_event,
                &mut client,
                &mut attach_input_processor,
                follow_target_id,
                Some(self_client_id),
                global,
                &attach_help_lines,
                &mut view_state,
            )
            .await?
            {
                AttachLoopControl::Continue => {}
                AttachLoopControl::Break(reason) => {
                    exit_reason = reason;
                    should_break = true;
                    break;
                }
            }
        }

        if should_break {
            break;
        }

        let _ = view_state.clear_expired_transient_status(Instant::now());

        let mut frame_needs_render = view_state.dirty.status_needs_redraw
            || view_state.dirty.full_pane_redraw
            || !view_state.dirty.pane_dirty_ids.is_empty();

        if view_state.dirty.layout_needs_refresh || view_state.cached_layout_state.is_none() {
            let previous_layout = view_state.cached_layout_state.clone();
            let layout_state = match client.attach_layout(view_state.attached_id).await {
                Ok(state) => state,
                Err(error) if is_attach_stream_closed_error(&error) => {
                    exit_reason = AttachExitReason::StreamClosed;
                    break;
                }
                Err(error) => return Err(map_attach_client_error(error)),
            };
            if view_state.cached_layout_state.as_ref() != Some(&layout_state) {
                frame_needs_render = true;
                let pane_ids = visible_scene_pane_ids(&layout_state.scene);
                for pane_id in pane_ids {
                    view_state.dirty.pane_dirty_ids.insert(pane_id);
                }
                match previous_layout {
                    None => {
                        view_state.dirty.full_pane_redraw = true;
                    }
                    Some(previous) => {
                        if previous.scene != layout_state.scene {
                            view_state.dirty.full_pane_redraw = true;
                        } else if previous.focused_pane_id != layout_state.focused_pane_id {
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(previous.focused_pane_id);
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(layout_state.focused_pane_id);
                        }
                    }
                }
                view_state.cached_layout_state = Some(layout_state);
            }
            view_state.dirty.layout_needs_refresh = false;
        }

        let Some(layout_state) = view_state.cached_layout_state.clone() else {
            continue;
        };

        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);

        let pane_ids = visible_scene_pane_ids(&layout_state.scene);
        view_state
            .pane_buffers
            .retain(|pane_id, _| pane_ids.iter().any(|id| id == pane_id));

        let chunks = match client
            .attach_pane_output_batch(view_state.attached_id, pane_ids.clone(), 8 * 1024)
            .await
        {
            Ok(chunks) => chunks,
            Err(error) if is_attach_stream_closed_error(&error) => {
                exit_reason = AttachExitReason::StreamClosed;
                break;
            }
            Err(error) => return Err(map_attach_client_error(error)),
        };

        for chunk in chunks {
            if chunk.data.is_empty() {
                continue;
            }
            let buffer = view_state.pane_buffers.entry(chunk.pane_id).or_default();
            append_pane_output(buffer, &chunk.data);
            view_state.dirty.pane_dirty_ids.insert(chunk.pane_id);
            frame_needs_render = true;
        }

        if !frame_needs_render {
            continue;
        }

        let help_scroll = view_state.help_overlay_scroll;
        render_attach_frame(
            &mut client,
            &mut view_state,
            &layout_state,
            follow_target_id,
            global,
            &attach_keymap,
            &attach_help_lines,
            help_scroll,
        )
        .await?;
    }

    drop(raw_mode_guard);
    restore_terminal_after_attach_ui()?;

    let _ = client.detach().await;
    if follow_target_id.is_some() {
        let _ = client.unfollow().await;
    }
    if let Some(message) = attach_exit_message(exit_reason) {
        println!("{message}");
    }
    Ok(0)
}

async fn handle_attach_runtime_action(
    client: &mut BmuxClient,
    action: RuntimeAction,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::NewWindow => {
            let window_id = client
                .new_window(Some(SessionSelector::ById(view_state.attached_id)), None)
                .await?;
            let _active_window_id = client
                .switch_window(
                    Some(SessionSelector::ById(view_state.attached_id)),
                    WindowSelector::ById(window_id),
                )
                .await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::NewSession => {
            let session_id = client.new_session(None).await?;
            let attach_info = open_attach_for_session(client, session_id).await?;
            view_state.attached_id = attach_info.session_id;
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_WELCOME_STATUS_TTL,
            );
            if !view_state.can_write {
                println!("read-only attach: input disabled");
            }
        }
        _ => {}
    }

    Ok(())
}

async fn handle_attach_ui_action(
    client: &mut BmuxClient,
    action: RuntimeAction,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::EnterWindowMode => {
            view_state.exit_scrollback();
            view_state.ui_mode = AttachUiMode::Window;
            view_state.set_transient_status(
                window_mode_enter_status(),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::ExitMode => {
            view_state.ui_mode = AttachUiMode::Normal;
        }
        RuntimeAction::EnterScrollMode => {
            if enter_attach_scrollback(view_state) {
                view_state.ui_mode = AttachUiMode::Normal;
            } else {
                view_state.set_transient_status(
                    ATTACH_SCROLLBACK_UNAVAILABLE_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::ExitScrollMode => {
            if view_state.selection_active() {
                clear_attach_selection(view_state, true);
            } else {
                view_state.exit_scrollback();
            }
        }
        RuntimeAction::ScrollUpLine => {
            step_attach_scrollback(view_state, -1);
        }
        RuntimeAction::ScrollDownLine => {
            step_attach_scrollback(view_state, 1);
        }
        RuntimeAction::ScrollUpPage => {
            step_attach_scrollback(
                view_state,
                -(attach_scrollback_page_size(view_state) as isize),
            );
        }
        RuntimeAction::ScrollDownPage => {
            step_attach_scrollback(view_state, attach_scrollback_page_size(view_state) as isize);
        }
        RuntimeAction::ScrollTop => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = max_attach_scrollback(view_state);
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::ScrollBottom => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = 0;
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::MoveCursorLeft => {
            move_attach_scrollback_cursor_horizontal(view_state, -1);
        }
        RuntimeAction::MoveCursorRight => {
            move_attach_scrollback_cursor_horizontal(view_state, 1);
        }
        RuntimeAction::MoveCursorUp => {
            move_attach_scrollback_cursor_vertical(view_state, -1);
        }
        RuntimeAction::MoveCursorDown => {
            move_attach_scrollback_cursor_vertical(view_state, 1);
        }
        RuntimeAction::BeginSelection => {
            if begin_attach_selection(view_state) {
                view_state.set_transient_status(
                    ATTACH_SELECTION_STARTED_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::CopyScrollback => {
            copy_attach_selection(view_state, false);
        }
        RuntimeAction::ConfirmScrollback => {
            confirm_attach_scrollback(view_state);
        }
        RuntimeAction::SessionPrev => {
            view_state.exit_scrollback();
            switch_attach_session_relative(client, view_state, -1).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::SessionNext => {
            view_state.exit_scrollback();
            switch_attach_session_relative(client, view_state, 1).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowPrev => {
            view_state.exit_scrollback();
            switch_attach_window_relative(client, view_state.attached_id, -1).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowNext => {
            view_state.exit_scrollback();
            switch_attach_window_relative(client, view_state.attached_id, 1).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto1 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 1).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto2 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 2).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto3 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 3).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto4 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 4).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto5 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 5).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto6 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 6).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto7 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 7).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto8 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 8).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowGoto9 => {
            view_state.exit_scrollback();
            switch_attach_window_number(client, view_state.attached_id, 9).await?;
            let status = attach_context_status(client, view_state.attached_id).await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowClose => {
            view_state.exit_scrollback();
            let _ = client
                .kill_window(
                    Some(SessionSelector::ById(view_state.attached_id)),
                    WindowSelector::Active,
                )
                .await?;
        }
        RuntimeAction::SplitFocusedVertical => {
            let _ = client
                .split_pane(
                    Some(SessionSelector::ById(view_state.attached_id)),
                    PaneSplitDirection::Vertical,
                )
                .await?;
        }
        RuntimeAction::SplitFocusedHorizontal => {
            let _ = client
                .split_pane(
                    Some(SessionSelector::ById(view_state.attached_id)),
                    PaneSplitDirection::Horizontal,
                )
                .await?;
        }
        RuntimeAction::FocusNext
        | RuntimeAction::FocusLeft
        | RuntimeAction::FocusRight
        | RuntimeAction::FocusUp
        | RuntimeAction::FocusDown => {
            let direction = if matches!(action, RuntimeAction::FocusLeft | RuntimeAction::FocusUp) {
                PaneFocusDirection::Prev
            } else {
                PaneFocusDirection::Next
            };
            let _ = client
                .focus_pane(
                    Some(SessionSelector::ById(view_state.attached_id)),
                    direction,
                )
                .await?;
        }
        RuntimeAction::IncreaseSplit
        | RuntimeAction::DecreaseSplit
        | RuntimeAction::ResizeLeft
        | RuntimeAction::ResizeRight
        | RuntimeAction::ResizeUp
        | RuntimeAction::ResizeDown => {
            let delta = if matches!(
                action,
                RuntimeAction::IncreaseSplit
                    | RuntimeAction::ResizeRight
                    | RuntimeAction::ResizeDown
            ) {
                1
            } else {
                -1
            };
            client
                .resize_pane(Some(SessionSelector::ById(view_state.attached_id)), delta)
                .await?;
        }
        RuntimeAction::CloseFocusedPane => {
            client
                .close_pane(Some(SessionSelector::ById(view_state.attached_id)))
                .await?;
        }
        RuntimeAction::NewWindow | RuntimeAction::NewSession => {
            handle_attach_runtime_action(client, action, view_state).await?;
        }
        _ => {}
    }

    Ok(())
}

fn enter_attach_scrollback(view_state: &mut AttachViewState) -> bool {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return false;
    };
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return false;
    };
    let (row, col) = buffer.parser.screen().cursor_position();
    view_state.scrollback_active = true;
    view_state.scrollback_offset = 0;
    view_state.scrollback_cursor = Some(AttachScrollbackCursor {
        row: usize::from(row).min(inner_h.saturating_sub(1)),
        col: usize::from(col).min(inner_w.saturating_sub(1)),
    });
    view_state.selection_anchor = None;
    true
}

fn begin_attach_selection(view_state: &mut AttachViewState) -> bool {
    if !view_state.scrollback_active {
        return false;
    }
    view_state.selection_anchor = attach_scrollback_cursor_absolute_position(view_state);
    view_state.selection_anchor.is_some()
}

fn clear_attach_selection(view_state: &mut AttachViewState, show_status: bool) {
    view_state.selection_anchor = None;
    if show_status {
        view_state.set_transient_status(
            ATTACH_SELECTION_CLEARED_STATUS,
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
    }
}

fn attach_scrollback_cursor_absolute_position(
    view_state: &AttachViewState,
) -> Option<AttachScrollbackPosition> {
    let cursor = view_state.scrollback_cursor?;
    Some(AttachScrollbackPosition {
        row: view_state.scrollback_offset.saturating_add(cursor.row),
        col: cursor.col,
    })
}

fn attach_selection_bounds(
    view_state: &AttachViewState,
) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
    let anchor = view_state.selection_anchor?;
    let head = attach_scrollback_cursor_absolute_position(view_state)?;
    Some(if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    })
}

fn step_attach_scrollback(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active {
        return;
    }
    let max_offset = max_attach_scrollback(view_state);
    view_state.scrollback_offset =
        adjust_attach_scrollback_offset(view_state.scrollback_offset, delta, max_offset);
    clamp_attach_scrollback_cursor(view_state);
}

fn move_attach_scrollback_cursor_horizontal(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active {
        return;
    }
    let Some((inner_w, _)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.col = adjust_scrollback_cursor_component(cursor.col, delta, inner_w.saturating_sub(1));
}

fn move_attach_scrollback_cursor_vertical(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active || delta == 0 {
        return;
    }
    let Some((_, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let max_offset = max_attach_scrollback(view_state);
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };

    if delta < 0 {
        for _ in 0..delta.unsigned_abs() {
            if cursor.row > 0 {
                cursor.row -= 1;
            } else if view_state.scrollback_offset < max_offset {
                view_state.scrollback_offset += 1;
            }
        }
    } else {
        for _ in 0..(delta as usize) {
            if cursor.row + 1 < inner_h {
                cursor.row += 1;
            } else if view_state.scrollback_offset > 0 {
                view_state.scrollback_offset -= 1;
            }
        }
    }

    clamp_attach_scrollback_cursor(view_state);
}

fn adjust_scrollback_cursor_component(current: usize, delta: isize, max_value: usize) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(max_value)
    }
}

fn copy_attach_selection(view_state: &mut AttachViewState, exit_after_copy: bool) {
    let Some(text) = selected_attach_text(view_state) else {
        if exit_after_copy {
            view_state.exit_scrollback();
        } else {
            view_state.set_transient_status(
                ATTACH_SELECTION_EMPTY_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        return;
    };

    match bmux_clipboard::copy_text(&text) {
        Ok(()) => {
            view_state.set_transient_status(
                ATTACH_SELECTION_COPIED_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if exit_after_copy {
                view_state.exit_scrollback();
            }
        }
        Err(error) => {
            view_state.set_transient_status(
                format_clipboard_error(&error),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
    }
}

fn confirm_attach_scrollback(view_state: &mut AttachViewState) {
    copy_attach_selection(view_state, true);
}

fn format_clipboard_error(error: &ClipboardError) -> String {
    match error {
        ClipboardError::BackendUnavailable { .. } => "clipboard backend unavailable".to_string(),
        ClipboardError::BackendFailed { message, .. } => {
            format!("clipboard copy failed: {message}")
        }
    }
}

fn selected_attach_text(view_state: &mut AttachViewState) -> Option<String> {
    let (start, end) = attach_selection_bounds(view_state)?;
    extract_attach_text(view_state, start, end)
}

fn extract_attach_text(
    view_state: &mut AttachViewState,
    start: AttachScrollbackPosition,
    end: AttachScrollbackPosition,
) -> Option<String> {
    let buffer = focused_attach_pane_buffer(view_state)?;
    let original_scrollback = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(start.row);
    let text = buffer.parser.screen().contents_between(
        0,
        start.col as u16,
        end.row.saturating_sub(start.row) as u16,
        end.col.saturating_add(1) as u16,
    );
    buffer
        .parser
        .screen_mut()
        .set_scrollback(original_scrollback);
    Some(text)
}

fn adjust_attach_scrollback_offset(current: usize, delta: isize, max_offset: usize) -> usize {
    if delta < 0 {
        current.saturating_add(delta.unsigned_abs()).min(max_offset)
    } else {
        current.saturating_sub(delta as usize)
    }
}

fn max_attach_scrollback(view_state: &mut AttachViewState) -> usize {
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return 0;
    };
    let previous = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(usize::MAX);
    let max_offset = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(previous);
    max_offset
}

fn clamp_attach_scrollback_cursor(view_state: &mut AttachViewState) {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        view_state.scrollback_cursor = None;
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.row = cursor.row.min(inner_h.saturating_sub(1));
    cursor.col = cursor.col.min(inner_w.saturating_sub(1));
}

fn attach_scrollback_page_size(view_state: &AttachViewState) -> usize {
    focused_attach_pane_inner_size(view_state)
        .map(|(_, inner_h)| inner_h)
        .unwrap_or(10)
}

fn focused_attach_pane_buffer(
    view_state: &mut AttachViewState,
) -> Option<&mut attach::state::PaneRenderBuffer> {
    let focused_pane_id = view_state.cached_layout_state.as_ref()?.focused_pane_id;
    view_state.pane_buffers.get_mut(&focused_pane_id)
}

fn focused_attach_pane_inner_size(view_state: &AttachViewState) -> Option<(usize, usize)> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    layout_state
        .scene
        .surfaces
        .iter()
        .find(|surface| surface.visible && surface.pane_id == Some(layout_state.focused_pane_id))
        .map(|surface| {
            (
                usize::from(surface.rect.w.saturating_sub(2).max(1)),
                usize::from(surface.rect.h.saturating_sub(2).max(1)),
            )
        })
}

async fn switch_attach_window_relative(
    client: &mut BmuxClient,
    session_id: Uuid,
    step: isize,
) -> std::result::Result<(), ClientError> {
    let windows = ordered_session_windows(client, session_id).await?;
    if windows.is_empty() {
        return Ok(());
    }

    let current_index = windows.iter().position(|window| window.active).unwrap_or(0);
    let len = windows.len() as isize;
    let mut target_index = current_index as isize + step;
    while target_index < 0 {
        target_index += len;
    }
    target_index %= len;

    let target_window_id = windows[target_index as usize].id;
    let _ = client
        .switch_window(
            Some(SessionSelector::ById(session_id)),
            WindowSelector::ById(target_window_id),
        )
        .await?;
    Ok(())
}

async fn switch_attach_session_relative(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    step: isize,
) -> std::result::Result<(), ClientError> {
    let sessions = client.list_sessions().await?;
    let Some(target_session_id) = relative_session_id(&sessions, view_state.attached_id, step)
    else {
        return Ok(());
    };

    let attach_info = open_attach_for_session(client, target_session_id).await?;
    view_state.attached_id = attach_info.session_id;
    view_state.can_write = attach_info.can_write;
    update_attach_viewport(client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    Ok(())
}

fn relative_session_id(
    sessions: &[SessionSummary],
    current_session_id: Uuid,
    step: isize,
) -> Option<Uuid> {
    if sessions.is_empty() {
        return None;
    }

    let current_index = sessions
        .iter()
        .position(|session| session.id == current_session_id)
        .unwrap_or(0);
    let len = sessions.len() as isize;
    let mut target_index = current_index as isize + step;
    while target_index < 0 {
        target_index += len;
    }
    target_index %= len;
    sessions
        .get(target_index as usize)
        .map(|session| session.id)
}

async fn switch_attach_window_number(
    client: &mut BmuxClient,
    session_id: Uuid,
    target_number: u32,
) -> std::result::Result<(), ClientError> {
    let _ = client
        .switch_window(
            Some(SessionSelector::ById(session_id)),
            WindowSelector::ByNumber(target_number),
        )
        .await?;
    Ok(())
}

async fn ordered_session_windows(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<Vec<bmux_ipc::WindowSummary>, ClientError> {
    let mut windows = client
        .list_windows(Some(SessionSelector::ById(session_id)))
        .await?;
    sort_attach_windows(&mut windows);
    Ok(windows)
}

fn sort_attach_windows(windows: &mut [bmux_ipc::WindowSummary]) {
    windows.sort_by(|left, right| {
        let left_rank = window_sort_rank(left);
        let right_rank = window_sort_rank(right);
        left_rank
            .cmp(&right_rank)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn window_sort_rank(window: &bmux_ipc::WindowSummary) -> (u32, String) {
    (
        window.number,
        window
            .name
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default(),
    )
}

fn window_has_default_title(number: u32, name: &str) -> bool {
    name == format!("window-{number}")
}

fn window_display_label(number: u32, name: Option<&str>) -> String {
    match name {
        Some(name) if !window_has_default_title(number, name) => format!("{number}:{name}"),
        _ => number.to_string(),
    }
}

fn window_summary_label(window: &bmux_ipc::WindowSummary) -> String {
    window_display_label(window.number, window.name.as_deref())
}

async fn build_attach_status_line_for_draw(
    client: &mut BmuxClient,
    session_id: Uuid,
    can_write: bool,
    ui_mode: AttachUiMode,
    scrollback_active: bool,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    quit_confirmation_pending: bool,
    help_overlay_open: bool,
    transient_status: Option<&str>,
    keymap: &Keymap,
) -> std::result::Result<String, ClientError> {
    let (cols, _) = terminal::size().unwrap_or((0, 0));
    if cols == 0 {
        return Ok(String::new());
    }

    let tabs = build_attach_tabs(client, session_id).await?;
    let session_label = resolve_attach_session_label(client, session_id).await?;
    let current_window_label = resolve_attach_window_label(client, session_id).await?;
    let mode_label = if help_overlay_open {
        "HELP"
    } else if scrollback_active {
        "SCROLL"
    } else {
        match ui_mode {
            AttachUiMode::Normal => "NORMAL",
            AttachUiMode::Window => "WINDOW",
        }
    };
    let role_label = if can_write { "write" } else { "read-only" };
    let follow_label = follow_target_id.map(|id| {
        if follow_global {
            format!("following {} (global)", short_uuid(id))
        } else {
            format!("following {}", short_uuid(id))
        }
    });
    let hint = if quit_confirmation_pending {
        "Quit session and all panes? [y/N]".to_string()
    } else if help_overlay_open {
        "Help overlay open | ? toggles | Esc/Enter close".to_string()
    } else if let Some(status) = transient_status {
        status.to_string()
    } else if scrollback_active {
        attach_scrollback_hint(keymap)
    } else {
        attach_mode_hint(ui_mode, keymap)
    };

    let status_line = build_attach_status_line(
        &session_label,
        &current_window_label,
        &tabs,
        mode_label,
        role_label,
        follow_label.as_deref(),
        &hint,
    );

    Ok(format_status_line_for_width(&status_line, cols))
}

fn format_status_line_for_width(status_line: &str, cols: u16) -> String {
    let width = usize::from(cols);
    let mut rendered = status_line.to_string();
    if rendered.len() > width {
        rendered.truncate(width);
    } else {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }
    rendered
}

fn attach_mode_hint(ui_mode: AttachUiMode, keymap: &Keymap) -> String {
    match ui_mode {
        AttachUiMode::Normal => {
            let window_mode = key_hint_or_unbound(keymap, RuntimeAction::EnterWindowMode);
            let detach = key_hint_or_unbound(keymap, RuntimeAction::Detach);
            let quit = key_hint_or_unbound(keymap, RuntimeAction::Quit);
            let help = key_hint_or_unbound(keymap, RuntimeAction::ShowHelp);
            format!("{window_mode} window mode | {detach} detach | {quit} quit | {help} help")
        }
        AttachUiMode::Window => {
            let session_prev = key_hint_or_unbound(keymap, RuntimeAction::SessionPrev);
            let session_next = key_hint_or_unbound(keymap, RuntimeAction::SessionNext);
            let prev = key_hint_or_unbound(keymap, RuntimeAction::WindowPrev);
            let next = key_hint_or_unbound(keymap, RuntimeAction::WindowNext);
            let goto_one = key_hint_or_unbound(keymap, RuntimeAction::WindowGoto1);
            let new_window = key_hint_or_unbound(keymap, RuntimeAction::NewWindow);
            let close = key_hint_or_unbound(keymap, RuntimeAction::WindowClose);
            let exit = key_hint_or_unbound(keymap, RuntimeAction::ExitMode);
            let help = key_hint_or_unbound(keymap, RuntimeAction::ShowHelp);
            format!(
                "{session_prev}/{session_next} prev/next session | {prev}/{next} prev/next window | {goto_one} goto-1 | {new_window} new | {close} close | {exit} exit | {help} help"
            )
        }
    }
}

fn initial_attach_status(keymap: &Keymap, can_write: bool) -> String {
    let help = key_hint_or_unbound(keymap, RuntimeAction::ShowHelp);
    let window_mode = key_hint_or_unbound(keymap, RuntimeAction::EnterWindowMode);
    if can_write {
        format!("{help} help | {window_mode} window mode | typing goes to pane")
    } else {
        format!("read-only attach | {help} help | {window_mode} window mode")
    }
}

fn window_mode_enter_status() -> String {
    "window mode: use window bindings, Esc/Enter exits".to_string()
}

fn attach_exit_message(reason: AttachExitReason) -> Option<&'static str> {
    match reason {
        AttachExitReason::Detached | AttachExitReason::Quit => None,
        AttachExitReason::StreamClosed => Some("attach ended unexpectedly: server stream closed"),
    }
}

fn attach_scrollback_hint(keymap: &Keymap) -> String {
    let exit = scroll_key_hint_or_unbound(keymap, RuntimeAction::ExitScrollMode);
    let confirm = scroll_key_hint_or_unbound(keymap, RuntimeAction::ConfirmScrollback);
    let left = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorLeft);
    let right = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorRight);
    let up = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorUp);
    let down = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorDown);
    let page_up = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollUpPage);
    let page_down = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollDownPage);
    let top = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollTop);
    let bottom = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollBottom);
    let select = scroll_key_hint_or_unbound(keymap, RuntimeAction::BeginSelection);
    let copy = scroll_key_hint_or_unbound(keymap, RuntimeAction::CopyScrollback);
    format!(
        "{up}/{down} line | {left}/{right} col | {page_up}/{page_down} page | {top}/{bottom} top/bottom | {select} select | {copy} copy | {confirm} copy+exit | {exit} cancel/exit scroll"
    )
}

fn scroll_key_hint_or_unbound(keymap: &Keymap, action: RuntimeAction) -> String {
    keymap
        .primary_scroll_binding_for_action(&action)
        .unwrap_or_else(|| "unbound".to_string())
}

fn key_hint_or_unbound(keymap: &Keymap, action: RuntimeAction) -> String {
    keymap
        .primary_binding_for_action(&action)
        .unwrap_or_else(|| "unbound".to_string())
}

fn queue_attach_status_line(stdout: &mut io::Stdout, status_line: &str) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    let rendered = format_status_line_for_width(status_line, cols);
    queue!(
        stdout,
        MoveTo(0, 0),
        Print("\x1b[7m"),
        Print(rendered),
        Print("\x1b[0m")
    )
    .context("failed queuing attach status line")
}

fn help_overlay_visible_rows(lines: &[String]) -> usize {
    let (_cols, rows) = terminal::size().unwrap_or((0, 0));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    height.saturating_sub(4).max(1)
}

fn adjust_help_overlay_scroll(
    current: usize,
    delta: isize,
    total_lines: usize,
    visible_rows: usize,
) -> usize {
    if total_lines == 0 {
        return 0;
    }
    let max_scroll = total_lines.saturating_sub(visible_rows.max(1));
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize)
    };
    next.min(max_scroll)
}

const fn help_overlay_accepts_key_kind(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn handle_help_overlay_key_event(
    key: &KeyEvent,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> bool {
    if !help_overlay_accepts_key_kind(key.kind) {
        return false;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            view_state.help_overlay_open = false;
            view_state.help_overlay_scroll = 0;
            view_state.dirty.status_needs_redraw = true;
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::PageUp => {
            let page = help_overlay_visible_rows(help_lines) as isize;
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::PageDown => {
            let page = help_overlay_visible_rows(help_lines) as isize;
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Home => {
            view_state.help_overlay_scroll = 0;
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::End => {
            let visible = help_overlay_visible_rows(help_lines);
            view_state.help_overlay_scroll = help_lines.len().saturating_sub(visible);
            view_state.dirty.full_pane_redraw = true;
            true
        }
        _ => false,
    }
}

fn help_overlay_surface(lines: &[String]) -> Option<bmux_ipc::AttachSurface> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols < 20 || rows < 6 {
        return None;
    }

    let content_width = lines
        .iter()
        .map(std::string::String::len)
        .max()
        .unwrap_or(0)
        .min(80);
    let width = (content_width + 4)
        .max(36)
        .min((cols as usize).saturating_sub(2));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    let x = ((cols as usize).saturating_sub(width)) / 2;
    let y = ((rows as usize).saturating_sub(height)) / 2;

    Some(bmux_ipc::AttachSurface {
        id: HELP_OVERLAY_SURFACE_ID,
        kind: bmux_ipc::AttachSurfaceKind::Overlay,
        layer: bmux_ipc::AttachLayer::Overlay,
        z: i32::MAX,
        rect: bmux_ipc::AttachRect {
            x: x as u16,
            y: y as u16,
            w: width as u16,
            h: height as u16,
        },
        opaque: true,
        visible: true,
        accepts_input: true,
        cursor_owner: false,
        pane_id: None,
    })
}

fn queue_attach_help_overlay(
    stdout: &mut io::Stdout,
    surface_meta: &bmux_ipc::AttachSurface,
    lines: &[String],
    scroll: usize,
) -> Result<()> {
    let width = usize::from(surface_meta.rect.w);
    let height = usize::from(surface_meta.rect.h);
    let x = usize::from(surface_meta.rect.x);
    let y = usize::from(surface_meta.rect.y);
    let body_rows = height.saturating_sub(4).max(1);
    let surface = AttachLayerSurface::new(
        PaneRect {
            x: surface_meta.rect.x,
            y: surface_meta.rect.y,
            w: surface_meta.rect.w,
            h: surface_meta.rect.h,
        },
        AttachLayer::Overlay,
        true,
    );
    let text_width = width.saturating_sub(4);

    let top = format!("+{}+", "-".repeat(width.saturating_sub(2)));
    queue!(stdout, MoveTo(x as u16, y as u16), Print(&top))
        .context("failed drawing help overlay top")?;

    let title = " bmux help ";
    let title_x = x + ((width.saturating_sub(title.len())) / 2);
    queue!(stdout, MoveTo(title_x as u16, y as u16), Print(title))
        .context("failed drawing help overlay title")?;

    for row in 1..height.saturating_sub(1) {
        let y_row = (y + row) as u16;
        queue!(
            stdout,
            MoveTo(x as u16, y_row),
            Print("|"),
            MoveTo((x + width - 1) as u16, y_row),
            Print("|")
        )
        .context("failed drawing help overlay border")?;
    }

    queue_layer_fill(stdout, surface).context("failed filling help overlay body")?;

    queue!(
        stdout,
        MoveTo(x as u16, (y + height - 1) as u16),
        Print(&top)
    )
    .context("failed drawing help overlay bottom")?;

    let header = "scope    chord                action";
    let header_rendered = opaque_row_text(header, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + 1) as u16),
        Print(header_rendered)
    )
    .context("failed drawing help overlay header")?;

    let start = scroll.min(lines.len().saturating_sub(body_rows));
    let end = (start + body_rows).min(lines.len());
    for (idx, line) in lines.iter().skip(start).take(body_rows).enumerate() {
        let rendered = opaque_row_text(line, text_width);
        let row = y + 2 + idx;
        if row >= y + height - 1 {
            break;
        }
        queue!(stdout, MoveTo((x + 2) as u16, row as u16), Print(rendered))
            .context("failed drawing help overlay entry")?;
    }

    let footer = format!(
        "j/k or ↑/↓ scroll | PgUp/PgDn | Esc close | {}-{} / {}",
        if lines.is_empty() { 0 } else { start + 1 },
        end,
        lines.len()
    );
    let footer_rendered = opaque_row_text(&footer, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + height - 2) as u16),
        Print(footer_rendered)
    )
    .context("failed drawing help overlay footer")?;

    Ok(())
}

async fn render_attach_frame(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    layout_state: &AttachLayoutState,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    keymap: &crate::input::Keymap,
    help_lines: &[String],
    help_scroll: usize,
) -> Result<()> {
    if view_state.dirty.status_needs_redraw {
        let now = Instant::now();
        view_state.cached_status_line = Some(
            build_attach_status_line_for_draw(
                client,
                view_state.attached_id,
                view_state.can_write,
                view_state.ui_mode,
                view_state.scrollback_active,
                follow_target_id,
                follow_global,
                view_state.quit_confirmation_pending,
                view_state.help_overlay_open,
                view_state.transient_status_text(now),
                keymap,
            )
            .await
            .map_err(map_attach_client_error)?,
        );
        view_state.dirty.status_needs_redraw = false;
    }

    let mut stdout = io::stdout();
    queue!(stdout, SavePosition).context("failed queuing cursor save for attach frame")?;
    if let Some(status_line) = view_state.cached_status_line.as_deref() {
        queue_attach_status_line(&mut stdout, status_line)?;
    }
    let cursor_state = render_attach_scene(
        &mut stdout,
        &layout_state.scene,
        &mut view_state.pane_buffers,
        &view_state.dirty.pane_dirty_ids,
        view_state.dirty.full_pane_redraw,
        view_state.scrollback_active,
        view_state.scrollback_offset,
        view_state.scrollback_cursor,
        view_state.selection_anchor,
    )?;
    if view_state.help_overlay_open {
        if let Some(help_surface) = help_overlay_surface(help_lines) {
            queue_attach_help_overlay(&mut stdout, &help_surface, help_lines, help_scroll)?;
        }
        apply_attach_cursor_state(&mut stdout, None, &mut view_state.last_cursor_state)?;
    } else {
        apply_attach_cursor_state(&mut stdout, cursor_state, &mut view_state.last_cursor_state)?;
    }
    stdout.flush().context("failed flushing attach frame")?;
    view_state.dirty.full_pane_redraw = false;
    view_state.dirty.pane_dirty_ids.clear();
    Ok(())
}

async fn build_attach_tabs(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<Vec<AttachTab>, ClientError> {
    let windows = ordered_session_windows(client, session_id).await?;
    Ok(windows
        .into_iter()
        .map(|window| AttachTab {
            label: window_summary_label(&window),
            active: window.active,
        })
        .collect())
}

async fn resolve_attach_window_label(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let windows = ordered_session_windows(client, session_id).await?;
    Ok(windows
        .iter()
        .find(|window| window.active)
        .map(window_summary_label)
        .unwrap_or_else(|| "-".to_string()))
}

async fn resolve_attach_session_label(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let sessions = client.list_sessions().await?;
    Ok(sessions
        .into_iter()
        .find(|session| session.id == session_id)
        .map(|session| session_summary_label(&session))
        .unwrap_or_else(|| format!("session-{}", short_uuid(session_id))))
}

fn session_summary_label(session: &bmux_ipc::SessionSummary) -> String {
    session
        .name
        .clone()
        .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
}

async fn attach_context_status(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let session_label = resolve_attach_session_label(client, session_id).await?;
    let window_label = resolve_attach_window_label(client, session_id).await?;
    Ok(format!("session: {session_label} | window: {window_label}"))
}

fn set_attach_context_status(
    view_state: &mut AttachViewState,
    status: String,
    now: Instant,
    ttl: Duration,
) {
    view_state.set_transient_status(status, now, ttl);
}

fn short_uuid(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

async fn resolve_follow_target_session(
    client: &mut BmuxClient,
    leader_client_id: Uuid,
) -> std::result::Result<Uuid, ClientError> {
    let clients = client.list_clients().await?;
    clients
        .into_iter()
        .find(|entry| entry.id == leader_client_id)
        .and_then(|entry| entry.selected_session_id)
        .ok_or(ClientError::UnexpectedResponse(
            "follow target has no selected session",
        ))
}

async fn open_attach_for_session(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<bmux_client::AttachOpenInfo, ClientError> {
    let grant = client
        .attach_grant(SessionSelector::ById(session_id))
        .await?;
    client.open_attach_stream_info(&grant).await
}

fn attach_keymap_from_config(config: &BmuxConfig) -> crate::input::Keymap {
    let (runtime_bindings, global_bindings) = filtered_attach_keybindings(config);
    let timeout_ms = config
        .keybindings
        .resolve_timeout()
        .map(|timeout| timeout.timeout_ms())
        .unwrap_or(None);
    match crate::input::Keymap::from_parts_with_scroll(
        &config.keybindings.prefix,
        timeout_ms,
        &runtime_bindings,
        &global_bindings,
        &config.keybindings.scroll,
    ) {
        Ok(keymap) => keymap,
        Err(error) => {
            eprintln!("bmux warning: invalid attach keymap config, using defaults ({error})");
            default_attach_keymap()
        }
    }
}

fn filtered_attach_keybindings(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let (runtime, global) = merged_runtime_keybindings(config);
    let runtime = normalize_attach_keybindings(runtime, "runtime");
    let mut global = normalize_attach_keybindings(global, "global");

    inject_attach_global_defaults(&mut global);
    (runtime, global)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachKeybindingScope {
    Runtime,
    Global,
}

impl AttachKeybindingScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachKeybindingEntry {
    scope: AttachKeybindingScope,
    chord: String,
    action: RuntimeAction,
    action_name: String,
}

fn effective_attach_keybindings(config: &BmuxConfig) -> Vec<AttachKeybindingEntry> {
    let (runtime, global) = filtered_attach_keybindings(config);
    let mut entries = Vec::new();

    for (chord, action_name) in runtime {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Runtime,
                chord,
                action,
                action_name,
            });
        }
    }
    for (chord, action_name) in global {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Global,
                chord,
                action,
                action_name,
            });
        }
    }

    entries.sort_by(|left, right| {
        left.scope
            .as_str()
            .cmp(right.scope.as_str())
            .then_with(|| left.chord.cmp(&right.chord))
    });
    entries
}

fn build_attach_help_lines(config: &BmuxConfig) -> Vec<String> {
    let keymap = attach_keymap_from_config(config);
    let help = key_hint_or_unbound(&keymap, RuntimeAction::ShowHelp);
    let window_mode = key_hint_or_unbound(&keymap, RuntimeAction::EnterWindowMode);
    let detach = key_hint_or_unbound(&keymap, RuntimeAction::Detach);
    let scroll = key_hint_or_unbound(&keymap, RuntimeAction::EnterScrollMode);
    let mut groups: Vec<(&str, Vec<AttachKeybindingEntry>)> = vec![
        ("Session", Vec::new()),
        ("Window", Vec::new()),
        ("Pane", Vec::new()),
        ("Mode", Vec::new()),
        ("Other", Vec::new()),
    ];

    for entry in effective_attach_keybindings(config) {
        let category = match entry.action {
            RuntimeAction::NewSession
            | RuntimeAction::SessionPrev
            | RuntimeAction::SessionNext
            | RuntimeAction::Detach
            | RuntimeAction::Quit => "Session",
            RuntimeAction::NewWindow
            | RuntimeAction::WindowPrev
            | RuntimeAction::WindowNext
            | RuntimeAction::WindowGoto1
            | RuntimeAction::WindowGoto2
            | RuntimeAction::WindowGoto3
            | RuntimeAction::WindowGoto4
            | RuntimeAction::WindowGoto5
            | RuntimeAction::WindowGoto6
            | RuntimeAction::WindowGoto7
            | RuntimeAction::WindowGoto8
            | RuntimeAction::WindowGoto9
            | RuntimeAction::WindowClose => "Window",
            RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane => "Pane",
            RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
            | RuntimeAction::ShowHelp => "Mode",
            _ => "Other",
        };

        if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| *name == category) {
            entries.push(entry);
        }
    }

    let mut lines = Vec::new();
    lines.push("Attach Help".to_string());
    lines.push(format!(
        "Normal mode sends typing to the pane. Use {window_mode} for window mode, {scroll} for scrollback, {detach} to detach, and {help} to toggle help."
    ));
    lines.push("Window mode captures window/session keys until Esc or Enter exits it.".to_string());
    lines.push(String::new());
    for (category, mut entries) in groups {
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|left, right| {
            left.scope
                .as_str()
                .cmp(right.scope.as_str())
                .then_with(|| left.chord.cmp(&right.chord))
        });
        lines.push(format!("-- {category} --"));
        for entry in entries {
            lines.push(format!(
                "[{:<7}] {:<20} {}",
                entry.scope.as_str(),
                entry.chord,
                entry.action_name
            ));
        }
        lines.push(String::new());
    }

    if lines.last().is_some_and(String::is_empty) {
        let _ = lines.pop();
    }
    lines
}

fn normalize_attach_keybindings(
    bindings: std::collections::BTreeMap<String, String>,
    scope: &str,
) -> std::collections::BTreeMap<String, String> {
    bindings
        .into_iter()
        .filter_map(
            |(chord, action_name)| match crate::input::parse_runtime_action_name(&action_name) {
                Ok(action) if is_attach_runtime_action(&action) => {
                    Some((chord, action_to_name(&action).to_string()))
                }
                Ok(_) => None,
                Err(error) => {
                    eprintln!(
                        "bmux warning: dropping invalid {scope} keybinding '{chord}' -> '{action_name}' ({error})"
                    );
                    None
                }
            },
        )
        .collect()
}

fn inject_attach_global_defaults(global: &mut std::collections::BTreeMap<String, String>) {
    let defaults = [
        ("ctrl+t", RuntimeAction::EnterWindowMode),
        ("escape", RuntimeAction::ExitMode),
        ("enter", RuntimeAction::ExitMode),
        ("shift+h", RuntimeAction::SessionPrev),
        ("shift+l", RuntimeAction::SessionNext),
        ("h", RuntimeAction::WindowPrev),
        ("l", RuntimeAction::WindowNext),
        ("1", RuntimeAction::WindowGoto1),
        ("2", RuntimeAction::WindowGoto2),
        ("3", RuntimeAction::WindowGoto3),
        ("4", RuntimeAction::WindowGoto4),
        ("5", RuntimeAction::WindowGoto5),
        ("6", RuntimeAction::WindowGoto6),
        ("7", RuntimeAction::WindowGoto7),
        ("8", RuntimeAction::WindowGoto8),
        ("9", RuntimeAction::WindowGoto9),
        ("x", RuntimeAction::WindowClose),
        ("n", RuntimeAction::NewWindow),
    ];

    for (key, action) in defaults {
        global
            .entry(key.to_string())
            .or_insert_with(|| action_to_name(&action).to_string());
    }
}

const fn is_attach_runtime_action(action: &RuntimeAction) -> bool {
    matches!(
        action,
        RuntimeAction::Detach
            | RuntimeAction::Quit
            | RuntimeAction::NewWindow
            | RuntimeAction::NewSession
            | RuntimeAction::SessionPrev
            | RuntimeAction::SessionNext
            | RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
            | RuntimeAction::WindowPrev
            | RuntimeAction::WindowNext
            | RuntimeAction::WindowGoto1
            | RuntimeAction::WindowGoto2
            | RuntimeAction::WindowGoto3
            | RuntimeAction::WindowGoto4
            | RuntimeAction::WindowGoto5
            | RuntimeAction::WindowGoto6
            | RuntimeAction::WindowGoto7
            | RuntimeAction::WindowGoto8
            | RuntimeAction::WindowGoto9
            | RuntimeAction::WindowClose
            | RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane
            | RuntimeAction::ShowHelp
    )
}

fn default_attach_keymap() -> crate::input::Keymap {
    let defaults = BmuxConfig::default();
    let (runtime_bindings, global_bindings) = filtered_attach_keybindings(&defaults);
    let timeout_ms = defaults
        .keybindings
        .resolve_timeout()
        .expect("default timeout config must be valid")
        .timeout_ms();
    crate::input::Keymap::from_parts_with_scroll(
        &defaults.keybindings.prefix,
        timeout_ms,
        &runtime_bindings,
        &global_bindings,
        &defaults.keybindings.scroll,
    )
    .expect("default attach keymap must be valid")
}

fn describe_timeout(timeout: &ResolvedTimeout) -> String {
    match timeout {
        ResolvedTimeout::Indefinite => "indefinite".to_string(),
        ResolvedTimeout::Exact(ms) => format!("exact ({ms}ms)"),
        ResolvedTimeout::Profile { name, ms } => format!("profile:{name} ({ms}ms)"),
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

async fn poll_attach_terminal_event(timeout: Duration) -> Result<Option<Event>> {
    tokio::task::spawn_blocking(move || {
        if event::poll(timeout).context("failed polling terminal events")? {
            let event = event::read().context("failed reading terminal event")?;
            return Ok(Some(event));
        }

        Ok(None)
    })
    .await
    .context("failed to join terminal event task")?
}

async fn update_attach_viewport(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    client.attach_set_viewport(session_id, cols, rows).await?;
    Ok(())
}

async fn hydrate_attach_state_from_snapshot(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    let AttachSnapshotState {
        session_id,
        window_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
        chunks,
    } = client
        .attach_snapshot(view_state.attached_id, ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE)
        .await?;

    view_state.cached_layout_state = Some(AttachLayoutState {
        session_id,
        window_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
    });
    view_state.pane_buffers.clear();
    if let Some(layout_state) = view_state.cached_layout_state.as_ref() {
        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);
    }
    for chunk in chunks {
        if chunk.data.is_empty() {
            continue;
        }
        let buffer = view_state.pane_buffers.entry(chunk.pane_id).or_default();
        append_pane_output(buffer, &chunk.data);
        view_state.dirty.pane_dirty_ids.insert(chunk.pane_id);
    }
    view_state.dirty.layout_needs_refresh = false;
    view_state.dirty.full_pane_redraw = true;
    view_state.dirty.status_needs_redraw = true;
    Ok(())
}

fn resize_attach_parsers_for_scene(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
) {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    resize_attach_parsers_for_scene_with_size(pane_buffers, scene, cols, rows);
}

fn resize_attach_parsers_for_scene_with_size(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
    cols: u16,
    rows: u16,
) {
    if cols == 0 || rows <= 1 {
        return;
    }

    for surface in &scene.surfaces {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        let rect = PaneRect {
            x: surface.rect.x.min(cols.saturating_sub(1)),
            y: surface.rect.y.min(rows.saturating_sub(1)),
            w: surface.rect.w.min(cols),
            h: surface
                .rect
                .h
                .min(rows.saturating_sub(surface.rect.y.min(rows.saturating_sub(1)))),
        };
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let inner_w = rect.w.saturating_sub(2).max(1);
        let inner_h = rect.h.saturating_sub(2).max(1);
        let buffer = pane_buffers.entry(pane_id).or_default();
        buffer.parser.screen_mut().set_size(inner_h, inner_w);
    }
}

async fn handle_attach_loop_event(
    event: AttachLoopEvent,
    client: &mut BmuxClient,
    attach_input_processor: &mut InputProcessor,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    global: bool,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    match event {
        AttachLoopEvent::Server(server_event) => {
            handle_attach_server_event(
                client,
                server_event,
                follow_target_id,
                self_client_id,
                global,
                view_state,
            )
            .await
        }
        AttachLoopEvent::Terminal(terminal_event) => {
            handle_attach_terminal_event(
                client,
                terminal_event,
                attach_input_processor,
                help_lines,
                view_state,
            )
            .await
        }
    }
}

async fn handle_attach_server_event(
    client: &mut BmuxClient,
    server_event: bmux_client::ServerEvent,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    _global: bool,
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    if is_attach_terminal_server_exit_event(&server_event, view_state.attached_id) {
        return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
    }

    match server_event {
        bmux_client::ServerEvent::FollowTargetChanged {
            follower_client_id,
            leader_client_id,
            session_id,
        } => {
            if Some(leader_client_id) != follow_target_id
                || Some(follower_client_id) != self_client_id
            {
                return Ok(AttachLoopControl::Continue);
            }
            let attach_info = open_attach_for_session(client, session_id)
                .await
                .map_err(map_attach_client_error)?;
            view_state.attached_id = attach_info.session_id;
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state)
                .await
                .map_err(map_attach_client_error)?;
            view_state.ui_mode = AttachUiMode::Normal;
            let status = attach_context_status(client, view_state.attached_id)
                .await
                .map_err(map_attach_client_error)?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if !view_state.can_write {
                println!("read-only attach: input disabled");
            }
        }
        bmux_client::ServerEvent::FollowTargetGone {
            former_leader_client_id,
            ..
        } if Some(former_leader_client_id) == follow_target_id => {
            println!("follow target disconnected; staying on current session");
        }
        bmux_client::ServerEvent::AttachViewChanged {
            session_id,
            components,
            ..
        } if session_id == view_state.attached_id => {
            apply_attach_view_change_components(&components, view_state);
        }
        bmux_client::ServerEvent::RoleChanged {
            session_id,
            client_id,
            role,
            ..
        } if session_id == view_state.attached_id && Some(client_id) == self_client_id => {
            apply_attach_role_change(role, view_state);
        }
        _ => {}
    }

    Ok(AttachLoopControl::Continue)
}

fn role_allows_attach_input(role: SessionRole) -> bool {
    matches!(role, SessionRole::Owner | SessionRole::Writer)
}

fn apply_attach_role_change(role: SessionRole, view_state: &mut AttachViewState) {
    let can_write = role_allows_attach_input(role);
    if view_state.can_write != can_write {
        view_state.can_write = can_write;
        view_state.dirty.status_needs_redraw = true;
    }
}

fn apply_attach_view_change_components(
    components: &[AttachViewComponent],
    view_state: &mut AttachViewState,
) {
    // Components are applied sequentially in server-provided order so future
    // fine-grained refresh behavior can build on earlier invalidation steps
    // without re-sorting or undoing prior effects.
    for component in components {
        match component {
            AttachViewComponent::Scene => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::SurfaceContent => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachViewComponent::Layout => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::Tabs => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::Status => {
                view_state.dirty.status_needs_redraw = true;
            }
        }
    }
}

fn is_attach_terminal_server_exit_event(
    event: &bmux_client::ServerEvent,
    attached_id: Uuid,
) -> bool {
    matches!(event, bmux_client::ServerEvent::SessionRemoved { id } if *id == attached_id)
}

async fn handle_attach_terminal_event(
    client: &mut BmuxClient,
    terminal_event: Event,
    attach_input_processor: &mut InputProcessor,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    if matches!(terminal_event, Event::Resize(_, _)) {
        update_attach_viewport(client, view_state.attached_id).await?;
    }

    let mut skip_attach_key_actions = false;
    if view_state.quit_confirmation_pending
        && let Event::Key(key) = &terminal_event
        && key.kind == KeyEventKind::Press
    {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                match client
                    .kill_session(SessionSelector::ById(view_state.attached_id))
                    .await
                {
                    Ok(_) => return Ok(AttachLoopControl::Break(AttachExitReason::Quit)),
                    Err(error) => {
                        let status = attach_quit_failure_status(&error);
                        view_state.set_transient_status(
                            status,
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                    }
                }
                view_state.quit_confirmation_pending = false;
                view_state.dirty.status_needs_redraw = true;
                skip_attach_key_actions = true;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Enter => {
                view_state.quit_confirmation_pending = false;
                view_state.dirty.status_needs_redraw = true;
                skip_attach_key_actions = true;
            }
            _ => {
                skip_attach_key_actions = true;
            }
        }
    }

    if skip_attach_key_actions {
        return Ok(AttachLoopControl::Continue);
    }

    if view_state.help_overlay_open
        && let Event::Key(key) = &terminal_event
        && handle_help_overlay_key_event(key, help_lines, view_state)
    {
        return Ok(AttachLoopControl::Continue);
    }

    for attach_action in
        attach_event_actions(&terminal_event, attach_input_processor, view_state.ui_mode)?
    {
        match attach_action {
            AttachEventAction::Detach => {
                return Ok(AttachLoopControl::Break(AttachExitReason::Detached));
            }
            AttachEventAction::Send(bytes) => {
                if view_state.help_overlay_open {
                    continue;
                }
                if view_state.can_write {
                    match client.attach_input(view_state.attached_id, bytes).await {
                        Ok(_) => {}
                        Err(error) if is_attach_stream_closed_error(&error) => {
                            return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
                        }
                        Err(error) => return Err(map_attach_client_error(error)),
                    }
                }
            }
            AttachEventAction::Runtime(action) => {
                if view_state.help_overlay_open {
                    continue;
                }
                if let Err(error) = handle_attach_runtime_action(client, action, view_state).await {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.layout_needs_refresh = true;
                    view_state.dirty.full_pane_redraw = true;
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::Ui(action) => {
                if matches!(action, RuntimeAction::ShowHelp) {
                    view_state.help_overlay_open = !view_state.help_overlay_open;
                    if !view_state.help_overlay_open {
                        view_state.help_overlay_scroll = 0;
                    }
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.full_pane_redraw = true;
                    continue;
                }
                if view_state.help_overlay_open {
                    if matches!(action, RuntimeAction::ExitMode)
                        || matches!(action, RuntimeAction::ForwardToPane(_))
                    {
                        view_state.help_overlay_open = false;
                        view_state.help_overlay_scroll = 0;
                        view_state.dirty.status_needs_redraw = true;
                        view_state.dirty.full_pane_redraw = true;
                    }
                    continue;
                }
                if matches!(action, RuntimeAction::Quit) {
                    view_state.quit_confirmation_pending = true;
                    view_state.dirty.status_needs_redraw = true;
                    continue;
                }
                if let Err(error) = handle_attach_ui_action(client, action, view_state).await {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.layout_needs_refresh = true;
                    view_state.dirty.full_pane_redraw = true;
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                view_state.dirty.status_needs_redraw = true;
            }
            AttachEventAction::Redraw => {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachEventAction::Ignore => {}
            AttachEventAction::WindowModeUnboundKey => {
                view_state.set_transient_status(
                    ATTACH_WINDOW_MODE_UNBOUND_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
    }

    Ok(AttachLoopControl::Continue)
}

fn restore_terminal_after_attach_ui() -> Result<()> {
    let mut stdout = io::stdout();
    queue!(
        stdout,
        Show,
        Print("\x1b[0m"),
        MoveTo(0, 0),
        Clear(ClearType::All),
        MoveTo(0, 0)
    )
    .context("failed restoring terminal after attach ui")?;
    stdout
        .flush()
        .context("failed flushing terminal restoration")
}

fn attach_event_actions(
    event: &Event,
    attach_input_processor: &mut InputProcessor,
    ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    match event {
        Event::Key(key) => attach_key_event_actions(key, attach_input_processor, ui_mode),
        Event::Resize(_, _) => Ok(vec![AttachEventAction::Redraw]),
        Event::Mouse(_) | Event::FocusGained | Event::FocusLost => {
            Ok(vec![AttachEventAction::Ignore])
        }
    }
}

fn attach_key_event_actions(
    key: &KeyEvent,
    attach_input_processor: &mut InputProcessor,
    ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    if key.kind != KeyEventKind::Press {
        return Ok(vec![AttachEventAction::Ignore]);
    }

    let actions = attach_input_processor.process_terminal_event(Event::Key(*key));
    Ok(actions
        .into_iter()
        .map(|action| match action {
            RuntimeAction::Detach => AttachEventAction::Detach,
            RuntimeAction::ForwardToPane(bytes) => {
                if ui_mode == AttachUiMode::Window {
                    AttachEventAction::WindowModeUnboundKey
                } else {
                    AttachEventAction::Send(bytes)
                }
            }
            RuntimeAction::NewWindow | RuntimeAction::NewSession => {
                if ui_mode == AttachUiMode::Window {
                    AttachEventAction::Runtime(action)
                } else if matches!(key.code, KeyCode::Char('n'))
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::SUPER)
                {
                    attach_key_event_to_bytes(key)
                        .map_or(AttachEventAction::Ignore, AttachEventAction::Send)
                } else {
                    AttachEventAction::Runtime(action)
                }
            }
            RuntimeAction::SessionPrev | RuntimeAction::SessionNext => {
                if ui_mode == AttachUiMode::Window {
                    AttachEventAction::Ui(action)
                } else {
                    attach_key_event_to_bytes(key)
                        .map_or(AttachEventAction::Ignore, AttachEventAction::Send)
                }
            }
            RuntimeAction::EnterWindowMode
            | RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane => {
                if ui_mode == AttachUiMode::Window {
                    AttachEventAction::WindowModeUnboundKey
                } else {
                    AttachEventAction::Ui(action)
                }
            }
            RuntimeAction::ExitMode
            | RuntimeAction::WindowPrev
            | RuntimeAction::WindowNext
            | RuntimeAction::WindowGoto1
            | RuntimeAction::WindowGoto2
            | RuntimeAction::WindowGoto3
            | RuntimeAction::WindowGoto4
            | RuntimeAction::WindowGoto5
            | RuntimeAction::WindowGoto6
            | RuntimeAction::WindowGoto7
            | RuntimeAction::WindowGoto8
            | RuntimeAction::WindowGoto9
            | RuntimeAction::WindowClose => {
                if ui_mode == AttachUiMode::Window {
                    AttachEventAction::Ui(action)
                } else {
                    attach_key_event_to_bytes(key)
                        .map_or(AttachEventAction::Ignore, AttachEventAction::Send)
                }
            }
            RuntimeAction::Quit => AttachEventAction::Ui(action),
            RuntimeAction::ShowHelp => AttachEventAction::Ui(action),
            RuntimeAction::ToggleSplitDirection
            | RuntimeAction::RestartFocusedPane
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::MoveCursorLeft
            | RuntimeAction::MoveCursorRight
            | RuntimeAction::MoveCursorUp
            | RuntimeAction::MoveCursorDown
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback => AttachEventAction::Ui(action),
        })
        .collect())
}

const fn is_attach_stream_closed_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::ServerError {
            code: bmux_ipc::ErrorCode::NotFound,
            ..
        }
    )
}

fn attach_key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    out.push((lower as u8 - b'a') + 1);
                    return Some(out);
                }
            }
            if c.is_ascii() {
                out.push(c as u8);
            } else {
                let mut buf = [0_u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
        KeyCode::Enter => {
            out.push(b'\r');
            Some(out)
        }
        KeyCode::Tab => {
            out.push(b'\t');
            Some(out)
        }
        KeyCode::Backspace => {
            out.push(0x7f);
            Some(out)
        }
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(vec![0x1b, b'[', b'A']),
        KeyCode::Down => Some(vec![0x1b, b'[', b'B']),
        KeyCode::Right => Some(vec![0x1b, b'[', b'C']),
        KeyCode::Left => Some(vec![0x1b, b'[', b'D']),
        _ => None,
    }
}

fn map_attach_client_error(error: ClientError) -> anyhow::Error {
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

fn map_cli_client_error(error: ClientError) -> anyhow::Error {
    map_client_connect_error(error)
}

async fn run_session_detach() -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-detach").await?;
    client.detach().await.map_err(map_cli_client_error)?;
    println!("detached");
    Ok(0)
}

async fn run_follow(target_client_id: &str, global: bool) -> Result<u8> {
    let target_client_id = parse_uuid_value(target_client_id, "target client id")?;
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-follow").await?;
    client
        .follow_client(target_client_id, global)
        .await
        .map_err(map_cli_client_error)?;
    println!(
        "following client: {}{}",
        target_client_id,
        if global { " (global)" } else { "" }
    );
    Ok(0)
}

async fn run_unfollow() -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-unfollow").await?;
    client.unfollow().await.map_err(map_cli_client_error)?;
    println!("follow stopped");
    Ok(0)
}

fn parse_session_selector(target: &str) -> SessionSelector {
    match Uuid::parse_str(target) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(target.to_string()),
    }
}

fn parse_uuid_value(value: &str, label: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("{label} must be a UUID, got '{value}'"))
}

const fn session_role_label(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Owner => "owner",
        SessionRole::Writer => "writer",
        SessionRole::Observer => "observer",
    }
}

async fn server_is_running() -> Result<bool> {
    probe_server_running().await
}

async fn probe_server_running() -> Result<bool> {
    Ok(fetch_server_status()
        .await?
        .is_some_and(|status| status.running))
}

async fn fetch_server_status() -> Result<Option<bmux_client::ServerStatusInfo>> {
    let connect = tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-status")).await;

    let mut client = match connect {
        Ok(Ok(client)) => client,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await {
        Ok(Ok(status)) => Ok(Some(status)),
        Ok(Err(_)) | Err(_) => Ok(None),
    }
}

async fn wait_for_server_running(timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let connect =
            tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-start-wait")).await;
        if let Ok(Ok(mut client)) = connect
            && let Ok(Ok(status)) =
                tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await
            && status.running
        {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }
    Ok(false)
}

async fn wait_until_server_stopped(timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let reconnect =
            tokio::time::timeout(SERVER_STATUS_TIMEOUT, connect_raw("bmux-cli-stop-check")).await;
        if reconnect.is_err() || matches!(reconnect, Ok(Err(_))) {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }

    Ok(false)
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_pid_running(pid)? {
            return Ok(true);
        }
        std::thread::sleep(SERVER_POLL_INTERVAL);
    }
    Ok(!is_pid_running(pid)?)
}

fn server_pid_file_path() -> PathBuf {
    bmux_config::ConfigPaths::default().server_pid_file()
}

fn write_server_pid_file(pid: u32) -> Result<()> {
    let path = server_pid_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating runtime dir {}", parent.display()))?;
    }
    std::fs::write(&path, pid.to_string())
        .with_context(|| format!("failed writing pid file {}", path.display()))
}

fn read_server_pid_file() -> Result<Option<u32>> {
    let path = server_pid_file_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading pid file {}", path.display()));
        }
    };

    if let Some(pid) = parse_pid_content(&content) {
        Ok(Some(pid))
    } else {
        let _ = remove_server_pid_file();
        Ok(None)
    }
}

fn remove_server_pid_file() -> Result<()> {
    let path = server_pid_file_path();
    let remove_pid_result = match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed removing pid file {}", path.display()))
        }
    };
    let remove_metadata_result = remove_server_runtime_metadata_file();
    remove_pid_result.and(remove_metadata_result)
}

fn try_kill_pid(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let status = ProcessCommand::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .context("failed to execute kill command")?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let status = ProcessCommand::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .status()
            .context("failed to execute taskkill command")?;
        return Ok(status.success());
    }
}

fn is_pid_running(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let status = ProcessCommand::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .context("failed to execute kill -0 command")?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let output = ProcessCommand::new("tasklist")
            .arg("/FI")
            .arg(filter)
            .output()
            .context("failed to execute tasklist command")?;
        if !output.status.success() {
            return Ok(false);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.lines().any(|line| line.contains(&pid.to_string())));
    }
}

async fn cleanup_stale_pid_file() -> Result<()> {
    let Some(pid) = read_server_pid_file()? else {
        return Ok(());
    };

    if !is_pid_running(pid)? && !probe_server_running().await? {
        remove_server_pid_file()?;
    }

    Ok(())
}

fn parse_pid_content(content: &str) -> Option<u32> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u32>().ok().filter(|pid| *pid > 0)
}

fn run_terminal_install_terminfo(yes: bool, check_only: bool) -> Result<u8> {
    let configured = BmuxConfig::load().map_or_else(
        |_| "bmux-256color".to_string(),
        |cfg| cfg.behavior.pane_term,
    );
    let is_installed = check_terminfo_available("bmux-256color") == Some(true);

    if check_only {
        if is_installed {
            println!("bmux-256color terminfo is installed");
            return Ok(0);
        }
        println!("bmux-256color terminfo is not installed");
        return Ok(1);
    }

    if is_installed {
        println!("bmux-256color terminfo is already installed");
        return Ok(0);
    }

    if !yes && io::stdin().is_terminal() {
        println!("bmux-256color terminfo is missing.");
        println!("Install now? [Y/n]");
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("failed reading install confirmation")?;
        let trimmed = answer.trim().to_ascii_lowercase();
        if trimmed == "n" || trimmed == "no" {
            println!("skipped terminfo installation");
            return Ok(0);
        }
    }

    install_bmux_terminfo()?;
    if check_terminfo_available("bmux-256color") == Some(true) {
        println!("installed terminfo entry: bmux-256color");
        if configured != "bmux-256color" {
            println!("note: current config pane_term is '{configured}'");
        }
        Ok(0)
    } else {
        anyhow::bail!("terminfo install completed but bmux-256color is still unavailable")
    }
}

fn run_terminal_doctor(
    as_json: bool,
    include_trace: bool,
    trace_limit: usize,
    trace_family: Option<TraceFamily>,
    trace_pane: Option<u16>,
) -> Result<u8> {
    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            println!(
                "bmux terminal doctor warning: failed to load config ({error}); using defaults"
            );
            BmuxConfig::default()
        }
    };

    let configured_term = config.behavior.pane_term.clone();
    let effective = resolve_pane_term(&configured_term);
    let protocol_profile = protocol_profile_for_terminal_profile(effective.profile);
    let last_declined_prompt_epoch_secs = last_prompt_decline_epoch_secs();
    let trace_data = if include_trace {
        load_protocol_trace(10_000)?
    } else {
        ProtocolTraceData::default()
    };
    let trace_events =
        filter_trace_events(&trace_data.events, trace_family, trace_pane, trace_limit);

    if as_json {
        let payload = serde_json::json!({
            "configured_pane_term": configured_term,
            "effective_pane_term": effective.pane_term,
            "terminal_profile": terminal_profile_name(effective.profile),
            "protocol_profile": protocol_profile_name(protocol_profile),
            "primary_da_reply": String::from_utf8_lossy(primary_da_for_profile(protocol_profile)),
            "secondary_da_reply": String::from_utf8_lossy(secondary_da_for_profile(protocol_profile)),
            "supported_queries": supported_query_names(),
            "fallback_chain": effective.fallback_chain,
            "terminfo_check": {
                "attempted": effective.terminfo_checked,
                "available": effective.terminfo_available,
            },
            "terminfo_checks": effective
                .terminfo_checks
                .iter()
                .map(|(term, available)| serde_json::json!({
                    "term": term,
                    "available": available,
                }))
                .collect::<Vec<_>>(),
            "warnings": effective.warnings,
            "terminfo_auto_install": {
                "policy": terminfo_auto_install_name(config.behavior.terminfo_auto_install),
                "prompt_cooldown_days": config.behavior.terminfo_prompt_cooldown_days,
                "last_declined_prompt_epoch_secs": last_declined_prompt_epoch_secs,
            },
            "trace": if include_trace {
                serde_json::json!({
                    "events": trace_events,
                    "limit": trace_limit,
                    "dropped": trace_data.dropped,
                    "applied_filters": {
                        "family": trace_family.map(trace_family_name),
                        "pane": trace_pane,
                    },
                })
            } else {
                serde_json::Value::Null
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed to encode terminal doctor json")?
        );
        return Ok(0);
    }

    println!("bmux terminal doctor");
    println!("configured pane TERM: {configured_term}");
    println!("effective pane TERM: {}", effective.pane_term);
    println!(
        "terminal profile: {}",
        terminal_profile_name(effective.profile)
    );
    println!(
        "protocol profile: {}",
        protocol_profile_name(protocol_profile)
    );
    println!(
        "primary DA reply: {}",
        String::from_utf8_lossy(primary_da_for_profile(protocol_profile))
    );
    println!(
        "secondary DA reply: {}",
        String::from_utf8_lossy(secondary_da_for_profile(protocol_profile))
    );
    println!(
        "terminfo auto-install policy: {} (cooldown {} days)",
        terminfo_auto_install_name(config.behavior.terminfo_auto_install),
        config.behavior.terminfo_prompt_cooldown_days
    );
    if let Some(epoch) = last_declined_prompt_epoch_secs {
        println!("last declined terminfo prompt (epoch secs): {epoch}");
    }
    println!("supported queries: {}", supported_query_names().join(", "));
    println!("fallback chain: {}", effective.fallback_chain.join(" -> "));
    if effective.terminfo_checked {
        println!(
            "terminfo available: {}",
            if effective.terminfo_available {
                "yes"
            } else {
                "no"
            }
        );
        for (term, available) in &effective.terminfo_checks {
            println!(
                "terminfo check {term}: {}",
                match available {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                }
            );
        }
    }
    for warning in effective.warnings {
        println!("warning: {warning}");
    }

    if include_trace {
        println!("trace events (latest {trace_limit}):");
        println!("trace dropped events: {}", trace_data.dropped);
        if trace_family.is_some() || trace_pane.is_some() {
            println!(
                "trace filters: family={} pane={}",
                trace_family.map_or("any", trace_family_name),
                trace_pane.map_or_else(|| "any".to_string(), |pane| pane.to_string())
            );
        }
        if trace_events.is_empty() {
            if trace_data.events.is_empty() {
                println!(
                    "  (no events found; enable behavior.protocol_trace_enabled and run a session)"
                );
            } else {
                println!("  (no events matched active filters)");
            }
        }
        for event in trace_events {
            let pane = event
                .pane_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());
            println!(
                "  [{}] pane={} {}:{} {} {}",
                event.timestamp_ms,
                pane,
                event.family,
                event.name,
                match event.direction {
                    ProtocolDirection::Query => "query",
                    ProtocolDirection::Reply => "reply",
                },
                event.decoded.replace('\u{1b}', "<ESC>")
            );
        }
    }

    Ok(0)
}

fn merged_runtime_keybindings(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let mut runtime = BmuxConfig::default().keybindings.runtime;
    runtime.extend(config.keybindings.runtime.clone());

    let mut global = BmuxConfig::default().keybindings.global;
    global.extend(config.keybindings.global.clone());

    (runtime, global)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ProtocolTraceFile {
    dropped: usize,
    events: Vec<ProtocolTraceEvent>,
}

#[derive(Debug, Default)]
struct ProtocolTraceData {
    dropped: usize,
    events: Vec<ProtocolTraceEvent>,
}

fn load_protocol_trace(limit: usize) -> Result<ProtocolTraceData> {
    let path = bmux_config::ConfigPaths::default().protocol_trace_file();
    if !path.exists() {
        return Ok(ProtocolTraceData::default());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading protocol trace file at {}", path.display()))?;
    let file: ProtocolTraceFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed parsing protocol trace file at {}", path.display()))?;
    if limit == 0 || file.events.len() <= limit {
        return Ok(ProtocolTraceData {
            dropped: file.dropped,
            events: file.events,
        });
    }
    let start = file.events.len().saturating_sub(limit);
    Ok(ProtocolTraceData {
        dropped: file.dropped,
        events: file.events.into_iter().skip(start).collect(),
    })
}

fn filter_trace_events(
    events: &[ProtocolTraceEvent],
    family: Option<TraceFamily>,
    pane: Option<u16>,
    limit: usize,
) -> Vec<ProtocolTraceEvent> {
    let mut filtered: Vec<ProtocolTraceEvent> = events
        .iter()
        .filter(|event| {
            let family_matches =
                family.is_none_or(|value| event.family == trace_family_name(value));
            let pane_matches = pane.is_none_or(|value| event.pane_id == Some(value));
            family_matches && pane_matches
        })
        .cloned()
        .collect();
    if limit > 0 && filtered.len() > limit {
        let start = filtered.len().saturating_sub(limit);
        filtered = filtered.split_off(start);
    }
    filtered
}

const fn trace_family_name(family: TraceFamily) -> &'static str {
    match family {
        TraceFamily::Csi => "csi",
        TraceFamily::Osc => "osc",
        TraceFamily::Dcs => "dcs",
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct TerminfoPromptStateFile {
    last_declined_epoch_secs: Option<u64>,
}

fn install_bmux_terminfo() -> Result<()> {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../terminfo/bmux-256color.terminfo");
    if !source.exists() {
        anyhow::bail!("terminfo source file not found at {}", source.display());
    }

    let output = ProcessCommand::new("tic")
        .arg("-x")
        .arg(&source)
        .output()
        .context("failed to execute tic")?;
    if !output.status.success() {
        anyhow::bail!(
            "tic failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

const fn terminfo_auto_install_name(policy: TerminfoAutoInstall) -> &'static str {
    match policy {
        TerminfoAutoInstall::Ask => "ask",
        TerminfoAutoInstall::Always => "always",
        TerminfoAutoInstall::Never => "never",
    }
}

fn last_prompt_decline_epoch_secs() -> Option<u64> {
    let path = bmux_config::ConfigPaths::default().terminfo_prompt_state_file();
    let bytes = std::fs::read(path).ok()?;
    let state: TerminfoPromptStateFile = serde_json::from_slice(&bytes).ok()?;
    state.last_declined_epoch_secs
}

struct PaneTermResolution {
    pane_term: String,
    profile: TerminalProfile,
    warnings: Vec<String>,
    terminfo_checked: bool,
    terminfo_available: bool,
    fallback_chain: Vec<String>,
    terminfo_checks: Vec<(String, Option<bool>)>,
}

fn resolve_pane_term(configured: &str) -> PaneTermResolution {
    resolve_pane_term_with_checker(configured, check_terminfo_available)
}

fn resolve_pane_term_with_checker<F>(configured: &str, mut checker: F) -> PaneTermResolution
where
    F: FnMut(&str) -> Option<bool>,
{
    let configured_trimmed = configured.trim();
    let configured_normalized = if configured_trimmed.is_empty() {
        "bmux-256color".to_string()
    } else {
        configured_trimmed.to_string()
    };

    let mut warnings = Vec::new();
    if configured_trimmed.is_empty() {
        warnings.push("behavior.pane_term is empty; falling back to bmux-256color".to_string());
    }

    let fallback_chain = vec!["xterm-256color".to_string(), "screen-256color".to_string()];
    let mut terminfo_checks = Vec::new();
    let mut pane_term = configured_normalized;

    let configured_check = checker(&pane_term);
    terminfo_checks.push((pane_term.clone(), configured_check));

    if configured_check == Some(false) {
        let mut selected_fallback = None;
        for candidate in &fallback_chain {
            if candidate == &pane_term {
                continue;
            }
            let check = checker(candidate);
            terminfo_checks.push((candidate.clone(), check));
            if check == Some(true) {
                selected_fallback = Some(candidate.clone());
                break;
            }
        }

        if let Some(fallback) = selected_fallback {
            warnings.push(format!(
                "pane TERM '{}' not installed; using '{}' (fallback chain: {})",
                pane_term,
                fallback,
                fallback_chain.join(", ")
            ));
            if pane_term == "bmux-256color" {
                warnings.push(
                    "install bmux terminfo with scripts/install-terminfo.sh to use bmux-256color"
                        .to_string(),
                );
            }
            pane_term = fallback;
        } else {
            warnings.push(format!(
                "pane TERM '{}' not installed and no fallback available (checked: {})",
                pane_term,
                fallback_chain.join(", ")
            ));
        }
    } else if configured_check.is_none() {
        warnings.push(format!(
            "could not verify terminfo for pane TERM '{pane_term}'; continuing without fallback checks"
        ));
    }

    let profile = profile_for_term(&pane_term);

    let effective_terminfo_available = terminfo_checks
        .iter()
        .find_map(|(term, available)| (term == &pane_term).then_some(*available))
        .flatten();

    if profile == TerminalProfile::Conservative {
        warnings.push(format!(
            "pane TERM '{pane_term}' uses conservative capability profile; compatibility depends on host terminfo"
        ));
    }

    PaneTermResolution {
        pane_term,
        profile,
        warnings,
        terminfo_checked: terminfo_checks
            .iter()
            .any(|(_, available)| available.is_some()),
        terminfo_available: effective_terminfo_available.unwrap_or(false),
        fallback_chain,
        terminfo_checks,
    }
}

fn profile_for_term(term: &str) -> TerminalProfile {
    match term {
        "bmux-256color" => TerminalProfile::Bmux256Color,
        "screen-256color" | "tmux-256color" => TerminalProfile::Screen256Color,
        "xterm-256color" => TerminalProfile::Xterm256Color,
        _ => TerminalProfile::Conservative,
    }
}

const fn terminal_profile_name(profile: TerminalProfile) -> &'static str {
    match profile {
        TerminalProfile::Bmux256Color => "bmux-256color",
        TerminalProfile::Screen256Color => "screen-256color-compatible",
        TerminalProfile::Xterm256Color => "xterm-256color-compatible",
        TerminalProfile::Conservative => "conservative",
    }
}

const fn protocol_profile_for_terminal_profile(profile: TerminalProfile) -> ProtocolProfile {
    match profile {
        TerminalProfile::Bmux256Color => ProtocolProfile::Bmux,
        TerminalProfile::Screen256Color => ProtocolProfile::Screen,
        TerminalProfile::Xterm256Color => ProtocolProfile::Xterm,
        TerminalProfile::Conservative => ProtocolProfile::Conservative,
    }
}

fn check_terminfo_available(term: &str) -> Option<bool> {
    let output = ProcessCommand::new("infocmp").arg(term).output().ok()?;
    Some(output.status.success())
}

fn run_keymap_doctor(as_json: bool) -> Result<u8> {
    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            println!("bmux keymap doctor warning: failed to load config ({error}); using defaults");
            BmuxConfig::default()
        }
    };
    let (runtime_bindings, global_bindings) = merged_runtime_keybindings(&config);
    let resolved_timeout = config
        .keybindings
        .resolve_timeout()
        .map_err(anyhow::Error::msg)
        .context("failed resolving keymap timeout")?;
    let keymap = crate::input::Keymap::from_parts(
        &config.keybindings.prefix,
        resolved_timeout.timeout_ms(),
        &runtime_bindings,
        &global_bindings,
    )
    .context("failed to compile keymap")?;

    let report = keymap.doctor_report();
    let attach_effective = effective_attach_keybindings(&config);

    if as_json {
        let payload = serde_json::json!({
            "prefix": config.keybindings.prefix,
            "timeout_ms": config.keybindings.timeout_ms,
            "timeout_profile": config.keybindings.timeout_profile,
            "timeout_profiles": config.keybindings.merged_timeout_profiles(),
            "resolved_timeout": match &resolved_timeout {
                ResolvedTimeout::Indefinite => serde_json::json!({
                    "mode": "indefinite"
                }),
                ResolvedTimeout::Exact(ms) => serde_json::json!({
                    "mode": "exact",
                    "ms": ms,
                }),
                ResolvedTimeout::Profile { name, ms } => serde_json::json!({
                    "mode": "profile",
                    "name": name,
                    "ms": ms,
                }),
            },
            "global": report
                .global
                .iter()
                .map(|binding| serde_json::json!({
                    "chord": binding.chord,
                    "action": binding.action,
                }))
                .collect::<Vec<_>>(),
            "runtime": report
                .runtime
                .iter()
                .map(|binding| serde_json::json!({
                    "chord": binding.chord,
                    "action": binding.action,
                }))
                .collect::<Vec<_>>(),
            "overlaps": report.overlaps,
            "attach_effective": attach_effective
                .iter()
                .map(|entry| serde_json::json!({
                    "scope": entry.scope.as_str(),
                    "chord": entry.chord,
                    "action": entry.action_name,
                }))
                .collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed to encode keymap doctor json")?
        );
        return Ok(0);
    }

    println!("bmux keymap doctor");
    println!("prefix: {}", config.keybindings.prefix);
    println!("timeout: {}", describe_timeout(&resolved_timeout));
    for line in keymap.doctor_lines() {
        println!("{line}");
    }

    println!("attach_effective:");
    for entry in attach_effective {
        println!(
            "  [{}] {} -> {}",
            entry.scope.as_str(),
            entry.chord,
            entry.action_name
        );
    }

    Ok(0)
}

fn init_logging(verbose: bool) {
    #[cfg(feature = "logging")]
    {
        let level = if verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::WARN
        };

        let _ = tracing_subscriber::fmt()
            .with_max_level(level)
            .with_target(false)
            .try_init();
    }

    #[cfg(not(feature = "logging"))]
    {
        let _ = verbose;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProtocolDirection, ProtocolTraceEvent, TerminalProfile, TraceFamily,
        apply_attach_role_change, apply_attach_view_change_components, attach_keymap_from_config,
        filter_trace_events, map_attach_client_error, map_cli_client_error,
        merged_runtime_keybindings, parse_pid_content, profile_for_term,
        protocol_profile_for_terminal_profile, resolve_pane_term_with_checker,
    };
    use crate::cli::Command;
    use crate::input::InputProcessor;
    use crate::runtime::attach::state::AttachViewState;
    use bmux_client::{AttachLayoutState, AttachOpenInfo, ClientError};
    use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout};
    use bmux_ipc::transport::IpcTransportError;
    use bmux_ipc::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        AttachViewComponent, ErrorCode, PaneLayoutNode, PaneSummary, SessionRole, SessionSummary,
    };
    use bmux_plugin::{PluginManifest, PluginRegistry};
    use crossterm::event::{
        KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-cli-plugin-test-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn plugin_manifest(id: &str, entry: &str) -> PluginManifest {
        PluginManifest::from_toml_str(&format!(
            "id = '{id}'\nname = 'Example'\nversion='0.1.0'\nentry='{entry}'\nrequired_capabilities=['bmux.commands']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n"
        ))
        .expect("manifest should parse")
    }

    fn plugin_manifest_with_commands(id: &str, entry: &str, commands: &str) -> PluginManifest {
        PluginManifest::from_toml_str(&format!(
            "id = '{id}'\nname = 'Example'\nversion='0.1.0'\nentry='{entry}'\nrequired_capabilities=['bmux.commands']\n{commands}\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n"
        ))
        .expect("manifest should parse")
    }

    fn attach_view_state_with_scrollback_fixture() -> AttachViewState {
        let pane_id = Uuid::from_u128(11);
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            session_id: Uuid::from_u128(12),
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            session_id: Uuid::from_u128(12),
            window_id: Uuid::from_u128(13),
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 0,
                name: None,
                focused: true,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id: Uuid::from_u128(12),
                window_id: Uuid::from_u128(13),
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 1,
                        w: 12,
                        h: 6,
                    },
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                }],
            },
        });
        let mut buffer = super::attach::state::PaneRenderBuffer::default();
        buffer.parser.screen_mut().set_size(4, 10);
        buffer.parser.process(b"one\ntwo\nthree\nfour\nfive\nsix\n");
        view_state.pane_buffers.insert(pane_id, buffer);
        view_state
    }

    #[test]
    fn validate_enabled_plugins_accepts_registered_plugin() {
        let dir = temp_dir();
        let plugin_dir = dir.join("example");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("example.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &plugin_dir.join("plugin.toml"),
                plugin_manifest("example.plugin", "example.dylib"),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("example.plugin".to_string());

        assert!(super::validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn validate_enabled_plugins_accepts_plugin_provided_capabilities() {
        let dir = temp_dir();
        let provider_dir = dir.join("windows");
        let dependent_dir = dir.join("consumer");
        fs::create_dir_all(&provider_dir).expect("provider dir should exist");
        fs::create_dir_all(&dependent_dir).expect("dependent dir should exist");
        fs::write(provider_dir.join("windows.dylib"), []).expect("provider entry should exist");
        fs::write(dependent_dir.join("consumer.dylib"), []).expect("dependent entry should exist");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &provider_dir.join("plugin.toml"),
                PluginManifest::from_toml_str(
                    "id='bmux.windows'\nname='Windows'\nversion='0.1.0'\nentry='windows.dylib'\nrequired_capabilities=['bmux.commands']\nprovided_capabilities=['bmux.windows.read','bmux.windows.write']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
                )
                .expect("provider manifest should parse"),
            )
            .expect("provider should register");
        registry
            .register_manifest(
                &dependent_dir.join("plugin.toml"),
                PluginManifest::from_toml_str(
                    "id='consumer.plugin'\nname='Consumer'\nversion='0.1.0'\nentry='consumer.dylib'\nrequired_capabilities=['bmux.windows.read']\n[[dependencies]]\nplugin_id='bmux.windows'\nversion_req='^0.1'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
                )
                .expect("dependent manifest should parse"),
            )
            .expect("dependent should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("bmux.windows".to_string());
        config.plugins.enabled.push("consumer.plugin".to_string());

        assert!(super::validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn validate_enabled_plugins_rejects_missing_plugin() {
        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("missing.plugin".to_string());

        let error = super::validate_enabled_plugins(&config, &PluginRegistry::new())
            .expect_err("validation should fail");
        assert!(error.to_string().contains("missing.plugin"));
    }

    #[test]
    fn validate_configured_plugins_discovers_plugins_from_default_layout() {
        let dir = temp_dir();
        let plugin_dir = dir.join("data").join("plugins").join("example");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("example.dylib"), []).expect("entry should be written");
        fs::write(
            plugin_dir.join("plugin.toml"),
            "id = 'example.plugin'\nname = 'Example'\nversion='0.1.0'\nentry='example.dylib'\nrequired_capabilities=['bmux.commands']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
        )
        .expect("manifest should be written");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("example.plugin".to_string());
        let paths = ConfigPaths::new(dir.join("config"), dir.join("runtime"), dir.join("data"));

        assert!(super::validate_configured_plugins(&config, &paths).is_ok());
    }

    #[test]
    fn runtime_cli_prefers_dynamic_session_plugin_aliases_over_static_cli_rejection() {
        let dir = temp_dir();
        let plugin_dir = dir.join("permissions");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("permissions.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &plugin_dir.join("plugin.toml"),
                plugin_manifest_with_commands(
                    "bmux.permissions",
                    "permissions.dylib",
                    "[[commands]]\nname='permissions'\npath=['permissions']\naliases=[[\"session\",\"permissions\"]]\nsummary='list'\nexecution='host_callback'\nexpose_in_cli=true\n[[commands.arguments]]\nname='session'\nkind='string'\nlong='session'\nrequired=true\n",
                ),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("bmux.permissions".to_string());
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("session"),
            OsString::from("permissions"),
            OsString::from("--session"),
            OsString::from("dev"),
        ];

        let parsed = super::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse plugin alias under session namespace");
        match parsed {
            super::ParsedRuntimeCli::Plugin {
                plugin_id,
                command_name,
                arguments,
                ..
            } => {
                assert_eq!(plugin_id, "bmux.permissions");
                assert_eq!(command_name, "permissions");
                assert_eq!(arguments, vec!["--session".to_string(), "dev".to_string()]);
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn plugin_lifecycle_context_uses_plugin_specific_settings() {
        let mut config = BmuxConfig::default();
        config
            .plugins
            .settings
            .insert("example.plugin".to_string(), "configured".into());

        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
        );
        let declaration = bmux_plugin::PluginDeclaration {
            id: bmux_plugin::PluginId::new("example.plugin").expect("id should parse"),
            display_name: "Example".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            native_abi: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            required_capabilities: std::collections::BTreeSet::from([bmux_plugin::HostScope::new(
                "bmux.commands",
            )
            .expect("capability should parse")]),
            provided_capabilities: std::collections::BTreeSet::from([bmux_plugin::HostScope::new(
                "bmux.windows.write",
            )
            .expect("capability should parse")]),
            provided_features: std::collections::BTreeSet::new(),
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle::default(),
        };
        let context = super::plugin_lifecycle_context(&config, &paths, &declaration);
        assert_eq!(context.plugin_id, "example.plugin");
        assert_eq!(context.connection.data_dir, "/data");
        assert_eq!(
            context.required_capabilities,
            vec!["bmux.commands".to_string()]
        );
        assert_eq!(
            context.provided_capabilities,
            vec!["bmux.windows.write".to_string()]
        );
        assert_eq!(
            context.settings.as_ref().and_then(|value| value.as_str()),
            Some("configured")
        );
    }

    #[test]
    fn plugin_command_context_includes_capability_sets() {
        let config = BmuxConfig::default();
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
        );
        let declaration = bmux_plugin::PluginDeclaration {
            id: bmux_plugin::PluginId::new("bmux.windows").expect("id should parse"),
            display_name: "Windows".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            native_abi: bmux_plugin::VersionRange::at_least(bmux_plugin::ApiVersion::new(1, 0)),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            required_capabilities: std::collections::BTreeSet::from([
                bmux_plugin::HostScope::new("bmux.commands").expect("capability should parse"),
                bmux_plugin::HostScope::new("bmux.sessions.read").expect("capability should parse"),
            ]),
            provided_capabilities: std::collections::BTreeSet::from([
                bmux_plugin::HostScope::new("bmux.windows.read").expect("capability should parse"),
                bmux_plugin::HostScope::new("bmux.windows.write").expect("capability should parse"),
            ]),
            provided_features: std::collections::BTreeSet::new(),
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle::default(),
        };

        let context = super::plugin_command_context(
            &config,
            &paths,
            &declaration,
            "new-window",
            &["--name".to_string(), "editor".to_string()],
        );

        assert_eq!(context.plugin_id, "bmux.windows");
        assert_eq!(context.command, "new-window");
        assert_eq!(
            context.required_capabilities,
            vec![
                "bmux.commands".to_string(),
                "bmux.sessions.read".to_string()
            ]
        );
        assert_eq!(
            context.provided_capabilities,
            vec![
                "bmux.windows.read".to_string(),
                "bmux.windows.write".to_string()
            ]
        );
    }

    #[test]
    fn plugin_system_event_uses_system_kind_and_name() {
        let event = super::plugin_system_event("server_started");
        assert_eq!(event.kind, bmux_plugin::PluginEventKind::System);
        assert_eq!(event.name, "server_started");
        assert_eq!(
            event
                .payload
                .get("product")
                .and_then(serde_json::Value::as_str),
            Some("bmux")
        );
    }

    #[test]
    fn plugin_event_from_server_event_maps_kind_and_payload() {
        let session_id = Uuid::from_u128(2);
        let event =
            super::plugin_event_from_server_event(&bmux_client::ServerEvent::WindowCreated {
                id: Uuid::from_u128(1),
                session_id,
                name: Some("editor".to_string()),
            })
            .expect("plugin event should build");
        let session_id_text = session_id.to_string();
        assert_eq!(event.kind, bmux_plugin::PluginEventKind::Window);
        assert_eq!(event.name, "window_created");
        assert!(event.payload.to_string().contains(&session_id_text));
    }

    #[test]
    fn built_in_handler_mapping_stays_in_sync_for_core_native_commands() {
        let command = Command::KillSession {
            target: "dev".to_string(),
            force_local: false,
        };
        assert_eq!(
            super::built_in_handler_for_command(&command),
            super::BuiltInHandlerId::KillSession
        );
    }

    #[test]
    fn pane_term_profile_mapping_is_stable() {
        assert_eq!(
            profile_for_term("bmux-256color"),
            TerminalProfile::Bmux256Color
        );
        assert_eq!(
            profile_for_term("screen-256color"),
            TerminalProfile::Screen256Color
        );
        assert_eq!(
            profile_for_term("tmux-256color"),
            TerminalProfile::Screen256Color
        );
        assert_eq!(
            profile_for_term("xterm-256color"),
            TerminalProfile::Xterm256Color
        );
        assert_eq!(
            profile_for_term("weird-term"),
            TerminalProfile::Conservative
        );
    }

    #[test]
    fn pane_term_falls_back_to_xterm_then_screen() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |term| match term {
            "bmux-256color" => Some(false),
            "xterm-256color" => Some(true),
            "screen-256color" => Some(true),
            _ => Some(false),
        });

        assert_eq!(resolved.pane_term, "xterm-256color");
        assert_eq!(resolved.profile, TerminalProfile::Xterm256Color);
    }

    #[test]
    fn pane_term_uses_screen_when_xterm_unavailable() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |term| match term {
            "bmux-256color" => Some(false),
            "xterm-256color" => Some(false),
            "screen-256color" => Some(true),
            _ => Some(false),
        });

        assert_eq!(resolved.pane_term, "screen-256color");
        assert_eq!(resolved.profile, TerminalProfile::Screen256Color);
    }

    #[test]
    fn pane_term_keeps_configured_when_no_fallback_available() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |_term| Some(false));

        assert_eq!(resolved.pane_term, "bmux-256color");
        assert!(
            resolved
                .warnings
                .iter()
                .any(|w| w.contains("no fallback available"))
        );
    }

    #[test]
    fn protocol_profile_mapping_is_stable() {
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Bmux256Color),
            super::ProtocolProfile::Bmux
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Xterm256Color),
            super::ProtocolProfile::Xterm
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Screen256Color),
            super::ProtocolProfile::Screen
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Conservative),
            super::ProtocolProfile::Conservative
        );
    }

    #[test]
    fn runtime_keybindings_deep_merge_defaults_and_overrides() {
        let mut config = BmuxConfig::default();
        config.keybindings.runtime.clear();
        config
            .keybindings
            .runtime
            .insert("o".to_string(), "quit".to_string());

        let (runtime, _global) = merged_runtime_keybindings(&config);

        assert_eq!(runtime.get("o"), Some(&"quit".to_string()));
        assert_eq!(
            runtime.get("%"),
            Some(&"split_focused_vertical".to_string())
        );
        assert_eq!(runtime.get("["), Some(&"enter_scroll_mode".to_string()));
    }

    #[test]
    fn describe_timeout_formats_resolved_timeout_states() {
        assert_eq!(
            super::describe_timeout(&ResolvedTimeout::Indefinite),
            "indefinite"
        );
        assert_eq!(
            super::describe_timeout(&ResolvedTimeout::Exact(275)),
            "exact (275ms)"
        );
        assert_eq!(
            super::describe_timeout(&ResolvedTimeout::Profile {
                name: "traditional".to_string(),
                ms: 450,
            }),
            "profile:traditional (450ms)"
        );
    }

    #[test]
    fn attach_view_change_components_mark_expected_dirty_flags() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.dirty.status_needs_redraw = false;
        view_state.dirty.layout_needs_refresh = false;
        view_state.dirty.full_pane_redraw = false;

        apply_attach_view_change_components(&[AttachViewComponent::Status], &mut view_state);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(!view_state.dirty.layout_needs_refresh);
        assert!(!view_state.dirty.full_pane_redraw);

        view_state.dirty.status_needs_redraw = false;
        apply_attach_view_change_components(&[AttachViewComponent::Tabs], &mut view_state);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.layout_needs_refresh);
        assert!(!view_state.dirty.full_pane_redraw);

        view_state.dirty.status_needs_redraw = false;
        view_state.dirty.layout_needs_refresh = false;
        apply_attach_view_change_components(
            &[AttachViewComponent::Scene, AttachViewComponent::Tabs],
            &mut view_state,
        );
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.layout_needs_refresh);
        assert!(view_state.dirty.full_pane_redraw);
    }

    #[test]
    fn attach_role_change_updates_write_status_only_when_needed() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.dirty.status_needs_redraw = false;

        apply_attach_role_change(SessionRole::Writer, &mut view_state);
        assert!(view_state.can_write);
        assert!(!view_state.dirty.status_needs_redraw);

        apply_attach_role_change(SessionRole::Observer, &mut view_state);
        assert!(!view_state.can_write);
        assert!(view_state.dirty.status_needs_redraw);

        view_state.dirty.status_needs_redraw = false;
        apply_attach_role_change(SessionRole::Observer, &mut view_state);
        assert!(!view_state.can_write);
        assert!(!view_state.dirty.status_needs_redraw);
    }

    #[test]
    fn trace_filtering_applies_family_and_pane_constraints() {
        let events = vec![
            ProtocolTraceEvent {
                timestamp_ms: 1,
                pane_id: Some(1),
                profile: "xterm".to_string(),
                family: "csi".to_string(),
                name: "csi_primary_da".to_string(),
                direction: ProtocolDirection::Query,
                raw_hex: "1b5b63".to_string(),
                decoded: "\u{1b}[c".to_string(),
            },
            ProtocolTraceEvent {
                timestamp_ms: 2,
                pane_id: Some(2),
                profile: "xterm".to_string(),
                family: "osc".to_string(),
                name: "osc_color_query".to_string(),
                direction: ProtocolDirection::Reply,
                raw_hex: "1b5d31303b3f".to_string(),
                decoded: "...".to_string(),
            },
            ProtocolTraceEvent {
                timestamp_ms: 3,
                pane_id: Some(2),
                profile: "xterm".to_string(),
                family: "csi".to_string(),
                name: "csi_primary_da".to_string(),
                direction: ProtocolDirection::Reply,
                raw_hex: "1b5b3f313b3263".to_string(),
                decoded: "...".to_string(),
            },
        ];

        let by_family = filter_trace_events(&events, Some(TraceFamily::Csi), None, 50);
        assert_eq!(by_family.len(), 2);

        let by_pane = filter_trace_events(&events, None, Some(2), 50);
        assert_eq!(by_pane.len(), 2);

        let both = filter_trace_events(&events, Some(TraceFamily::Csi), Some(2), 50);
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].timestamp_ms, 3);
    }

    #[test]
    fn parse_pid_content_accepts_positive_pid() {
        assert_eq!(parse_pid_content("123\n"), Some(123));
    }

    #[test]
    fn parse_pid_content_rejects_invalid_values() {
        assert_eq!(parse_pid_content(""), None);
        assert_eq!(parse_pid_content("0"), None);
        assert_eq!(parse_pid_content("abc"), None);
    }

    #[test]
    fn attach_exit_events_ignore_session_scoped_client_detach() {
        let session_id = uuid::Uuid::new_v4();
        assert!(super::is_attach_terminal_server_exit_event(
            &bmux_client::ServerEvent::SessionRemoved { id: session_id },
            session_id,
        ));
        assert!(!super::is_attach_terminal_server_exit_event(
            &bmux_client::ServerEvent::ClientDetached { id: session_id },
            session_id,
        ));
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
    fn destructive_op_error_formats_owner_role_guidance() {
        let message = super::format_destructive_op_error(
            "session",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "owner role required for this operation".to_string(),
            },
            false,
        );

        assert!(message.contains("requires the session owner role"));
        assert!(message.contains("bmux session permissions <session>"));
    }

    #[test]
    fn destructive_op_error_formats_force_local_guidance() {
        let message = super::format_destructive_op_error(
            "window",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "force-local is only allowed for the server owner principal".to_string(),
            },
            true,
        );

        assert!(message.contains("--force-local"));
        assert!(message.contains("bmux server whoami-principal"));
    }

    #[test]
    fn attach_quit_failure_status_is_actionable_for_owner_errors() {
        let status = super::attach_quit_failure_status(&ClientError::ServerError {
            code: ErrorCode::InvalidRequest,
            message: "owner role required for this operation".to_string(),
        });

        assert_eq!(status, "quit requires session owner");
    }

    #[test]
    fn server_event_name_maps_known_variants() {
        assert_eq!(
            super::server_event_name(&bmux_client::ServerEvent::ServerStarted),
            "server_started"
        );
        assert_eq!(
            super::server_event_name(&bmux_client::ServerEvent::ClientDetached {
                id: uuid::Uuid::new_v4()
            }),
            "client_detached"
        );
    }

    #[test]
    fn attach_key_event_action_detaches_on_prefix_d() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));
        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('d'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], super::AttachEventAction::Detach));
    }

    #[test]
    fn attach_key_event_action_ctrl_d_forwards_to_pane() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('d'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(actions[0], super::AttachEventAction::Send(ref bytes) if bytes == &[0x04])
        );
    }

    #[test]
    fn attach_key_event_action_encodes_char_input() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('x'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], super::AttachEventAction::Send(ref bytes) if bytes == b"x"));
    }

    #[test]
    fn attach_key_event_action_maps_prefixed_runtime_defaults() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));

        let prefix = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(prefix.is_empty());

        let new_window = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('c'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            new_window.first(),
            Some(super::AttachEventAction::Runtime(
                crate::input::RuntimeAction::NewWindow
            ))
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let split_vertical = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('%'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            split_vertical.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::SplitFocusedVertical
            ))
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let quit = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('q'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            quit.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::Quit
            ))
        ));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let new_session = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('C'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            new_session.first(),
            Some(super::AttachEventAction::Runtime(
                crate::input::RuntimeAction::NewSession
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_enters_window_mode_with_ctrl_t() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('t'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::EnterWindowMode
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_h_as_ui_only_in_window_mode() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));

        let normal_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('h'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            normal_actions.first(),
            Some(super::AttachEventAction::Send(bytes)) if bytes.as_slice() == b"h"
        ));

        let window_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('h'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Window,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            window_actions.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::WindowPrev
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_enter_scroll_mode_to_ui() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('['),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::EnterScrollMode
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_shift_h_as_session_ui_only_in_window_mode() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));

        let normal_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('H'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            normal_actions.first(),
            Some(super::AttachEventAction::Send(bytes)) if bytes.as_slice() == b"H"
        ));

        let window_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('H'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Window,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            window_actions.first(),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::SessionPrev
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_n_to_pane_in_normal_mode() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));

        let normal_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('n'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            normal_actions.first(),
            Some(super::AttachEventAction::Send(bytes)) if bytes.as_slice() == b"n"
        ));
    }

    #[test]
    fn attach_key_event_action_routes_n_to_new_window_in_window_mode() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));

        let window_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('n'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Window,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            window_actions.first(),
            Some(super::AttachEventAction::Runtime(
                crate::input::RuntimeAction::NewWindow
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_eats_unbound_plain_key_in_window_mode() {
        let mut processor = InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()));

        let window_actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('z'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Window,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            window_actions.first(),
            Some(super::AttachEventAction::WindowModeUnboundKey)
        ));
    }

    #[test]
    fn attach_keybindings_allow_global_override_of_default_window_mode_key() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .global
            .insert("ctrl+t".to_string(), "new_session".to_string());

        let mut processor = InputProcessor::new(attach_keymap_from_config(&config));
        let actions = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('t'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(super::AttachEventAction::Runtime(
                crate::input::RuntimeAction::NewSession
            ))
        ));
    }

    #[test]
    fn attach_mode_hint_reflects_remapped_normal_mode_keys() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .runtime
            .insert("d".to_string(), "quit".to_string());
        config
            .keybindings
            .global
            .insert("ctrl+t".to_string(), "new_session".to_string());
        config
            .keybindings
            .runtime
            .insert("z".to_string(), "detach".to_string());
        config
            .keybindings
            .global
            .insert("ctrl+w".to_string(), "enter_window_mode".to_string());

        let keymap = attach_keymap_from_config(&config);
        let hint = super::attach_mode_hint(super::AttachUiMode::Normal, &keymap);
        assert!(hint.contains("Ctrl-W window mode"));
        assert!(hint.contains("Ctrl-A z detach"));
        assert!(hint.contains("Ctrl-A d quit"));
    }

    #[test]
    fn attach_mode_hint_reflects_window_mode_overrides() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .global
            .insert("shift+h".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("shift+l".to_string(), "detach".to_string());
        config
            .keybindings
            .global
            .insert("h".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("l".to_string(), "detach".to_string());
        config
            .keybindings
            .global
            .insert("1".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("n".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("x".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("escape".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("enter".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("u".to_string(), "window_prev".to_string());
        config
            .keybindings
            .global
            .insert("i".to_string(), "window_next".to_string());
        config
            .keybindings
            .global
            .insert("0".to_string(), "window_goto_1".to_string());
        config
            .keybindings
            .global
            .insert("m".to_string(), "new_window".to_string());
        config
            .keybindings
            .global
            .insert("k".to_string(), "window_close".to_string());
        config
            .keybindings
            .global
            .insert("ctrl+g".to_string(), "exit_mode".to_string());

        let keymap = attach_keymap_from_config(&config);
        let hint = super::attach_mode_hint(super::AttachUiMode::Window, &keymap);
        assert!(hint.contains("unbound/unbound prev/next session"));
        assert!(hint.contains("u/i prev/next window"));
        assert!(hint.contains("0 goto-1"));
        assert!(hint.contains("m new"));
        assert!(hint.contains("k close"));
        assert!(hint.contains("Ctrl-G exit"));
    }

    #[test]
    fn relative_session_id_wraps_between_sessions() {
        let session_a = Uuid::from_u128(1);
        let session_b = Uuid::from_u128(2);
        let sessions = vec![
            SessionSummary {
                id: session_a,
                name: Some("a".to_string()),
                window_count: 1,
                client_count: 1,
            },
            SessionSummary {
                id: session_b,
                name: Some("b".to_string()),
                window_count: 1,
                client_count: 1,
            },
        ];

        assert_eq!(
            super::relative_session_id(&sessions, session_a, -1),
            Some(session_b)
        );
        assert_eq!(
            super::relative_session_id(&sessions, session_a, 1),
            Some(session_b)
        );
        assert_eq!(
            super::relative_session_id(&sessions, session_b, 1),
            Some(session_a)
        );
    }

    #[test]
    fn adjust_attach_scrollback_offset_clamps_within_bounds() {
        assert_eq!(super::adjust_attach_scrollback_offset(0, -1, 4), 1);
        assert_eq!(super::adjust_attach_scrollback_offset(3, -10, 4), 4);
        assert_eq!(super::adjust_attach_scrollback_offset(4, 1, 4), 3);
        assert_eq!(super::adjust_attach_scrollback_offset(1, 50, 4), 0);
    }

    #[test]
    fn adjust_scrollback_cursor_component_clamps_within_bounds() {
        assert_eq!(super::adjust_scrollback_cursor_component(0, -1, 5), 0);
        assert_eq!(super::adjust_scrollback_cursor_component(2, -1, 5), 1);
        assert_eq!(super::adjust_scrollback_cursor_component(2, 10, 5), 5);
    }

    #[test]
    fn enter_attach_scrollback_initializes_cursor_from_live_position() {
        let mut view_state = attach_view_state_with_scrollback_fixture();

        assert!(super::enter_attach_scrollback(&mut view_state));
        assert!(view_state.scrollback_active);
        assert_eq!(view_state.scrollback_offset, 0);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 2 })
        );
    }

    #[test]
    fn move_attach_scrollback_cursor_vertical_scrolls_at_viewport_edges() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));

        super::move_attach_scrollback_cursor_vertical(&mut view_state, -1);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 2, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 0);

        super::move_attach_scrollback_cursor_vertical(&mut view_state, -3);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 0, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 1);

        super::move_attach_scrollback_cursor_vertical(&mut view_state, 1);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 1, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 1);
    }

    #[test]
    fn move_attach_scrollback_cursor_horizontal_updates_column() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));

        super::move_attach_scrollback_cursor_horizontal(&mut view_state, 3);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 5 })
        );

        super::move_attach_scrollback_cursor_horizontal(&mut view_state, -10);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 0 })
        );
    }

    #[test]
    fn begin_attach_selection_uses_absolute_cursor_position() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));
        view_state.scrollback_offset = 2;

        assert!(super::begin_attach_selection(&mut view_state));
        assert_eq!(
            view_state.selection_anchor,
            Some(super::attach::state::AttachScrollbackPosition { row: 5, col: 2 })
        );
    }

    #[test]
    fn clear_attach_selection_removes_anchor() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));
        assert!(super::begin_attach_selection(&mut view_state));

        super::clear_attach_selection(&mut view_state, false);
        assert_eq!(view_state.selection_anchor, None);
    }

    #[test]
    fn selected_attach_text_extracts_multiline_range() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));
        view_state.selection_anchor =
            Some(super::attach::state::AttachScrollbackPosition { row: 1, col: 1 });
        view_state.scrollback_cursor =
            Some(super::attach::state::AttachScrollbackCursor { row: 3, col: 2 });
        view_state.scrollback_offset = 0;

        assert_eq!(
            super::selected_attach_text(&mut view_state),
            Some("four\n     five\n".to_string())
        );
    }

    #[test]
    fn confirm_attach_scrollback_exits_when_no_selection() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(super::enter_attach_scrollback(&mut view_state));

        super::confirm_attach_scrollback(&mut view_state);
        assert!(!view_state.scrollback_active);
    }

    #[test]
    fn attach_scrollback_hint_uses_default_bindings() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let hint = super::attach_scrollback_hint(&keymap);

        assert!(hint.contains("select"));
        assert!(hint.contains("copy"));
        assert!(hint.contains("page"));
        assert!(hint.contains("top/bottom"));
        assert!(hint.contains("exit scroll"));
    }

    #[test]
    fn attach_keybindings_keep_focus_next_pane_binding() {
        let (runtime, _global) = super::filtered_attach_keybindings(&BmuxConfig::default());
        assert_eq!(runtime.get("o"), Some(&"focus_next_pane".to_string()));
    }

    #[test]
    fn attach_key_event_action_maps_show_help_to_ui() {
        let config = BmuxConfig::default();
        let keymap = super::attach_keymap_from_config(&config);
        let mut processor = InputProcessor::new(keymap);

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let help_question = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('?'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let _ = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let help_shift_slash = super::attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('/'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            super::AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            help_question.first().or_else(|| help_shift_slash.first()),
            Some(super::AttachEventAction::Ui(
                crate::input::RuntimeAction::ShowHelp
            ))
        ));
    }

    #[test]
    fn effective_attach_keybindings_include_scope_and_canonical_action_names() {
        let entries = super::effective_attach_keybindings(&BmuxConfig::default());
        assert!(entries.iter().any(|entry| {
            entry.scope == super::AttachKeybindingScope::Runtime
                && entry.chord == "o"
                && entry.action_name == "focus_next_pane"
                && entry.action == crate::input::RuntimeAction::FocusNext
        }));
        assert!(entries.iter().any(|entry| {
            entry.scope == super::AttachKeybindingScope::Global
                && entry.chord == "ctrl+t"
                && entry.action_name == "enter_window_mode"
                && entry.action == crate::input::RuntimeAction::EnterWindowMode
        }));
    }

    #[test]
    fn adjust_help_overlay_scroll_clamps_to_bounds() {
        assert_eq!(super::adjust_help_overlay_scroll(0, -10, 20, 5), 0);
        assert_eq!(super::adjust_help_overlay_scroll(0, 3, 20, 5), 3);
        assert_eq!(super::adjust_help_overlay_scroll(17, 10, 20, 5), 15);
        assert_eq!(super::adjust_help_overlay_scroll(4, -2, 20, 5), 2);
        assert_eq!(super::adjust_help_overlay_scroll(0, 4, 0, 5), 0);
    }

    #[test]
    fn help_overlay_repeat_navigation_is_handled() {
        let mut view_state = super::AttachViewState::new(bmux_client::AttachOpenInfo {
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.help_overlay_open = true;
        let lines = (0..200)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>();

        let handled = super::handle_help_overlay_key_event(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Down,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Repeat,
            ),
            &lines,
            &mut view_state,
        );
        assert!(handled);
        assert!(view_state.help_overlay_scroll > 0);
    }

    #[test]
    fn help_overlay_release_is_ignored() {
        let mut view_state = super::AttachViewState::new(bmux_client::AttachOpenInfo {
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.help_overlay_open = true;
        view_state.help_overlay_scroll = 5;
        let lines = (0..200)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>();

        let handled = super::handle_help_overlay_key_event(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Down,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Release,
            ),
            &lines,
            &mut view_state,
        );
        assert!(!handled);
        assert_eq!(view_state.help_overlay_scroll, 5);
    }

    #[test]
    fn build_attach_help_lines_groups_entries_by_category() {
        let lines = super::build_attach_help_lines(&BmuxConfig::default());
        assert_eq!(lines.first().map(String::as_str), Some("Attach Help"));
        assert!(lines[1].contains("Normal mode sends typing to the pane"));
        assert!(lines.iter().any(|line| line == "-- Session --"));
        assert!(lines.iter().any(|line| line == "-- Window --"));
        assert!(lines.iter().any(|line| line == "-- Pane --"));
        assert!(lines.iter().any(|line| line == "-- Mode --"));
    }

    #[test]
    fn attach_exit_message_suppresses_normal_detach_and_formats_stream_close() {
        assert_eq!(
            super::attach_exit_message(super::AttachExitReason::Detached),
            None
        );
        assert_eq!(
            super::attach_exit_message(super::AttachExitReason::Quit),
            None
        );
        assert_eq!(
            super::attach_exit_message(super::AttachExitReason::StreamClosed),
            Some("attach ended unexpectedly: server stream closed")
        );
    }

    #[test]
    fn initial_attach_status_mentions_help_and_window_mode() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let status = super::initial_attach_status(&keymap, true);
        assert!(status.contains("help"));
        assert!(status.contains("window mode"));
        assert!(status.contains("typing goes to pane"));
    }

    #[test]
    fn sort_attach_windows_prefers_window_number_then_name() {
        let mut windows = vec![
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000003")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                number: 3,
                name: Some("editor".to_string()),
                active: false,
            },
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                number: 10,
                name: Some("window-10".to_string()),
                active: false,
            },
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000002")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                number: 2,
                name: Some("window-2".to_string()),
                active: true,
            },
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000004")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                number: 99,
                name: Some("zeta".to_string()),
                active: false,
            },
        ];

        super::sort_attach_windows(&mut windows);

        let ordered_names: Vec<String> = windows
            .into_iter()
            .map(|window| super::window_summary_label(&window))
            .collect();
        assert_eq!(ordered_names, vec!["2", "3:editor", "10", "99:zeta"]);
    }

    #[test]
    fn resize_attach_parsers_applies_layout_size_before_snapshot_bytes() {
        let pane_id = uuid::Uuid::new_v4();
        let scene = bmux_ipc::AttachScene {
            session_id: uuid::Uuid::new_v4(),
            window_id: uuid::Uuid::new_v4(),
            focus: bmux_ipc::AttachFocusTarget::Pane { pane_id },
            surfaces: vec![bmux_ipc::AttachSurface {
                id: pane_id,
                kind: bmux_ipc::AttachSurfaceKind::Pane,
                layer: bmux_ipc::AttachLayer::Pane,
                z: 0,
                rect: bmux_ipc::AttachRect {
                    x: 0,
                    y: 1,
                    w: 120,
                    h: 49,
                },
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(pane_id, super::attach::state::PaneRenderBuffer::default());

        super::resize_attach_parsers_for_scene_with_size(&mut pane_buffers, &scene, 120, 50);

        let buffer = pane_buffers
            .get_mut(&pane_id)
            .expect("pane buffer should exist");
        super::append_pane_output(&mut *buffer, b"\x1b[999;999H");
        let (row, col) = buffer.parser.screen().cursor_position();

        assert_eq!(row, 46, "cursor row should clamp to pane inner height");
        assert_eq!(col, 117, "cursor col should clamp to pane inner width");
    }
}
