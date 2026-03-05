use crate::cli::{
    Cli, Command, KeymapCommand, RoleValue, ServerCommand, SessionCommand, TerminalCommand,
    TraceFamily, WindowCommand,
};
use crate::input::{InputProcessor, Keymap, RuntimeAction};
use crate::status::{AttachTab, build_attach_status_line};
use anyhow::{Context, Result};
use bmux_client::{AttachLayoutState, BmuxClient, ClientError};
use bmux_config::{BmuxConfig, TerminfoAutoInstall};
use bmux_ipc::{
    PaneFocusDirection, PaneLayoutNode, PaneSplitDirection, SessionRole, SessionSelector,
    SessionSummary, WindowSelector, transport::IpcTransportError,
};
use bmux_server::BmuxServer;
use clap::Parser;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::terminal::{Clear, ClearType};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

mod terminal_protocol;
use terminal_protocol::{
    ProtocolDirection, ProtocolProfile, ProtocolTraceEvent, primary_da_for_profile,
    protocol_profile_name, secondary_da_for_profile, supported_query_names,
};

const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(200);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_millis(5000);
const ATTACH_IO_POLL_INTERVAL: Duration = Duration::from_millis(15);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ServerRuntimeMetadata {
    pid: u32,
    version: String,
    build_id: String,
    executable_path: String,
    started_at_epoch_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProfile {
    Bmux256Color,
    Screen256Color,
    Xterm256Color,
    Conservative,
}

pub(crate) async fn run() -> Result<u8> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    if let Some(command) = &cli.command {
        return run_command(command).await;
    }

    run_default_server_attach().await
}

async fn run_default_server_attach() -> Result<u8> {
    ensure_server_running_for_default_attach().await?;
    warn_stale_server_build_on_default_attach()?;
    let mut client = BmuxClient::connect_default("bmux-cli-default-attach")
        .await
        .map_err(map_cli_client_error)?;
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

fn warn_stale_server_build_on_default_attach() -> Result<()> {
    let Some(metadata) = read_server_runtime_metadata()? else {
        return Ok(());
    };
    let current_build = current_cli_build_id()?;
    if metadata.build_id != current_build {
        eprintln!(
            "bmux warning: running server build differs from current CLI build; restart with `bmux server stop`"
        );
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
            .map(|permission| permission.role)
            .unwrap_or(SessionRole::Observer);
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
        Command::NewSession { name } => run_session_new(name.clone()).await,
        Command::ListSessions { json } => run_session_list(*json).await,
        Command::ListClients { json } => run_client_list(*json).await,
        Command::Permissions {
            session,
            json,
            watch,
        } => run_permissions_list(session, *json, *watch).await,
        Command::Grant {
            session,
            client,
            role,
        } => run_grant_role(session, client, *role).await,
        Command::Revoke { session, client } => run_revoke_role(session, client).await,
        Command::KillSession {
            target,
            force_local,
        } => run_session_kill(target, *force_local).await,
        Command::KillAllSessions { force_local } => run_session_kill_all(*force_local).await,
        Command::Attach {
            target,
            follow,
            global,
        } => run_session_attach(target.as_deref(), follow.as_deref(), *global).await,
        Command::Detach => run_session_detach().await,
        Command::NewWindow { session, name } => {
            run_window_new(session.as_ref(), name.clone()).await
        }
        Command::ListWindows { session, json } => run_window_list(session.as_ref(), *json).await,
        Command::KillWindow {
            target,
            session,
            force_local,
        } => run_window_kill(target, session.as_ref(), *force_local).await,
        Command::KillAllWindows {
            session,
            force_local,
        } => run_window_kill_all(session.as_ref(), *force_local).await,
        Command::SwitchWindow { target, session } => {
            run_window_switch(target, session.as_ref()).await
        }
        Command::Follow {
            target_client_id,
            global,
        } => run_follow(target_client_id, *global).await,
        Command::Unfollow => run_unfollow().await,
        Command::Session { command } => match command {
            SessionCommand::New { name } => run_session_new(name.clone()).await,
            SessionCommand::List { json } => run_session_list(*json).await,
            SessionCommand::Clients { json } => run_client_list(*json).await,
            SessionCommand::Permissions {
                session,
                json,
                watch,
            } => run_permissions_list(session, *json, *watch).await,
            SessionCommand::Grant {
                session,
                client,
                role,
            } => run_grant_role(session, client, *role).await,
            SessionCommand::Revoke { session, client } => run_revoke_role(session, client).await,
            SessionCommand::Kill {
                target,
                force_local,
            } => run_session_kill(target, *force_local).await,
            SessionCommand::KillAll { force_local } => run_session_kill_all(*force_local).await,
            SessionCommand::Attach {
                target,
                follow,
                global,
            } => run_session_attach(target.as_deref(), follow.as_deref(), *global).await,
            SessionCommand::Detach => run_session_detach().await,
            SessionCommand::Follow {
                target_client_id,
                global,
            } => run_follow(target_client_id, *global).await,
            SessionCommand::Unfollow => run_unfollow().await,
        },
        Command::Window { command } => match command {
            WindowCommand::New { session, name } => {
                run_window_new(session.as_ref(), name.clone()).await
            }
            WindowCommand::List { session, json } => run_window_list(session.as_ref(), *json).await,
            WindowCommand::Kill {
                target,
                session,
                force_local,
            } => run_window_kill(target, session.as_ref(), *force_local).await,
            WindowCommand::KillAll {
                session,
                force_local,
            } => run_window_kill_all(session.as_ref(), *force_local).await,
            WindowCommand::Switch { target, session } => {
                run_window_switch(target, session.as_ref()).await
            }
        },
        Command::Server { command } => match command {
            ServerCommand::Start {
                daemon,
                foreground_internal,
            } => run_server_start(*daemon, *foreground_internal).await,
            ServerCommand::Status { json } => run_server_status(*json).await,
            ServerCommand::WhoamiPrincipal { json } => run_server_whoami_principal(*json).await,
            ServerCommand::Save => run_server_save().await,
            ServerCommand::Restore { dry_run, yes } => run_server_restore(*dry_run, *yes).await,
            ServerCommand::Stop => run_server_stop().await,
        },
        Command::Keymap { command } => match command {
            KeymapCommand::Doctor { json } => run_keymap_doctor(*json),
        },
        Command::Terminal { command } => match command {
            TerminalCommand::Doctor {
                json,
                trace,
                trace_limit,
                trace_family,
                trace_pane,
            } => run_terminal_doctor(*json, *trace, *trace_limit, *trace_family, *trace_pane),
            TerminalCommand::InstallTerminfo { yes, check } => {
                run_terminal_install_terminfo(*yes, *check)
            }
        },
    }
}

async fn run_server_start(daemon: bool, foreground_internal: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    if server_is_running().await? {
        println!("bmux server is already running");
        return Ok(1);
    }

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

    let server = BmuxServer::from_default_paths();
    write_server_pid_file(std::process::id())?;
    write_server_runtime_metadata(std::process::id())?;
    let run_result = server.run().await;
    let _ = remove_server_pid_file();
    run_result?;
    Ok(0)
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
        return Ok(if payload.running { 0 } else { 1 });
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
    let mut client = BmuxClient::connect_default("bmux-cli-server-whoami-principal")
        .await
        .map_err(map_cli_client_error)?;
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
    let mut client = BmuxClient::connect_default("bmux-cli-server-save")
        .await
        .map_err(map_cli_client_error)?;
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
        let mut client = BmuxClient::connect_default("bmux-cli-server-restore-dry-run")
            .await
            .map_err(map_cli_client_error)?;
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

    let mut client = BmuxClient::connect_default("bmux-cli-server-restore-apply")
        .await
        .map_err(map_cli_client_error)?;
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
    let connect = tokio::time::timeout(
        SERVER_STATUS_TIMEOUT,
        BmuxClient::connect_default("bmux-cli-status-events"),
    )
    .await;

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

fn server_event_name(event: &bmux_client::ServerEvent) -> &'static str {
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
    }
}

async fn run_server_stop() -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let graceful_stopped = match tokio::time::timeout(
        SERVER_STOP_TIMEOUT,
        BmuxClient::connect_default("bmux-cli-stop"),
    )
    .await
    {
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
    let mut client = BmuxClient::connect_default("bmux-cli-new-session")
        .await
        .map_err(map_cli_client_error)?;
    let session_id = client
        .new_session(name)
        .await
        .map_err(map_cli_client_error)?;
    println!("created session: {session_id}");
    Ok(0)
}

async fn run_session_list(as_json: bool) -> Result<u8> {
    let mut client = BmuxClient::connect_default("bmux-cli-list-sessions")
        .await
        .map_err(map_cli_client_error)?;
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
    let mut client = BmuxClient::connect_default("bmux-cli-list-clients")
        .await
        .map_err(map_cli_client_error)?;
    let self_id = client.whoami().await.map_err(map_cli_client_error)?;
    let clients = client.list_clients().await.map_err(map_cli_client_error)?;
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

    println!(
        "ID                                   SELF ROLE      SELECTED_SESSION                     FOLLOWING_CLIENT                     GLOBAL"
    );
    for client in clients {
        let role = client.session_role.map_or("-", session_role_label);
        let selected_session = client
            .selected_session_id
            .map_or_else(|| "-".to_string(), |id| id.to_string());
        let following_client = client
            .following_client_id
            .map_or_else(|| "-".to_string(), |id| id.to_string());
        println!(
            "{:<36} {:<4} {:<9} {:<36} {:<36} {}",
            client.id,
            if client.id == self_id { "yes" } else { "no" },
            role,
            selected_session,
            following_client,
            if client.following_global { "yes" } else { "no" }
        );
    }

    Ok(0)
}

async fn run_permissions_list(session: &str, as_json: bool, watch: bool) -> Result<u8> {
    let selector = parse_session_selector(session);

    if watch {
        let mut client = BmuxClient::connect_default("bmux-cli-watch-permissions")
            .await
            .map_err(map_cli_client_error)?;

        println!("watching permissions for session '{session}' (Ctrl-C to stop)");
        let mut last_permissions: Option<Vec<bmux_ipc::SessionPermissionSummary>> = None;

        loop {
            let permissions = client
                .list_permissions(selector.clone())
                .await
                .map_err(map_cli_client_error)?;

            if last_permissions.as_ref() != Some(&permissions) {
                render_permissions_table(&permissions);
                last_permissions = Some(permissions);
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let mut client = BmuxClient::connect_default("bmux-cli-list-permissions")
        .await
        .map_err(map_cli_client_error)?;
    let permissions = client
        .list_permissions(selector)
        .await
        .map_err(map_cli_client_error)?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&permissions)
                .context("failed to encode permissions json")?
        );
        return Ok(0);
    }

    render_permissions_table(&permissions);

    Ok(0)
}

fn render_permissions_table(permissions: &[bmux_ipc::SessionPermissionSummary]) {
    if permissions.is_empty() {
        println!("no explicit role assignments");
        return;
    }

    println!("CLIENT_ID                            ROLE");
    for permission in permissions {
        println!(
            "{:<36} {}",
            permission.client_id,
            session_role_label(permission.role)
        );
    }
}

async fn run_grant_role(session: &str, client: &str, role: RoleValue) -> Result<u8> {
    let selector = parse_session_selector(session);
    let client_id = parse_uuid_value(client, "client id")?;
    let mut api = BmuxClient::connect_default("bmux-cli-grant-role")
        .await
        .map_err(map_cli_client_error)?;
    api.grant_role(selector, client_id, session_role_from_value(role))
        .await
        .map_err(map_cli_client_error)?;

    println!(
        "granted role {} to client {}",
        session_role_label(session_role_from_value(role)),
        client_id
    );
    Ok(0)
}

async fn run_revoke_role(session: &str, client: &str) -> Result<u8> {
    let selector = parse_session_selector(session);
    let client_id = parse_uuid_value(client, "client id")?;
    let mut api = BmuxClient::connect_default("bmux-cli-revoke-role")
        .await
        .map_err(map_cli_client_error)?;
    api.revoke_role(selector, client_id)
        .await
        .map_err(map_cli_client_error)?;

    println!("revoked explicit role for client {client_id}");
    Ok(0)
}

async fn run_session_kill(target: &str, force_local: bool) -> Result<u8> {
    let selector = parse_session_selector(target);
    let mut client = BmuxClient::connect_default("bmux-cli-kill-session")
        .await
        .map_err(map_cli_client_error)?;
    let killed_id = client
        .kill_session_with_options(selector, force_local)
        .await
        .map_err(map_cli_client_error)?;
    println!("killed session: {killed_id}");
    Ok(0)
}

async fn run_session_kill_all(force_local: bool) -> Result<u8> {
    let mut client = BmuxClient::connect_default("bmux-cli-kill-all-sessions")
        .await
        .map_err(map_cli_client_error)?;
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(0);
    }

    let mut killed_count = 0usize;
    let mut failed_count = 0usize;
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
                let mapped_error = map_cli_client_error(error);
                eprintln!("failed killing session {}: {mapped_error:#}", session.id);
            }
        }
    }

    println!("kill-all-sessions complete: killed {killed_count}, failed {failed_count}");
    Ok(if failed_count == 0 { 0 } else { 1 })
}

async fn run_session_attach(
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    let client = BmuxClient::connect_default("bmux-cli-attach")
        .await
        .map_err(map_attach_client_error)?;
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

    let self_client_id = if follow_target_id.is_some() {
        Some(client.whoami().await.map_err(map_attach_client_error)?)
    } else {
        None
    };

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

    let mut view_state = AttachViewState::new(attach_info.clone());

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
                self_client_id,
                global,
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

        let mut frame_needs_render =
            view_state.dirty.status_needs_redraw || view_state.dirty.layout_needs_refresh;

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
            view_state.cached_layout_state = Some(layout_state);
        }
        view_state.dirty.layout_needs_refresh = false;

        let Some(layout_state) = view_state.cached_layout_state.clone() else {
            continue;
        };

        let mut pane_ids = Vec::new();
        collect_pane_ids(&layout_state.layout_root, &mut pane_ids);
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
            frame_needs_render = true;
        }

        if !frame_needs_render {
            continue;
        }

        render_attach_frame(
            &mut client,
            &mut view_state,
            &layout_state,
            follow_target_id,
            global,
            &attach_keymap,
        )
        .await?;
    }

    drop(raw_mode_guard);
    restore_terminal_after_attach_ui()?;

    let _ = client.detach().await;
    if follow_target_id.is_some() {
        let _ = client.unfollow().await;
    }
    if exit_reason == AttachExitReason::StreamClosed {
        println!("attach stream closed");
    }
    println!("detached");
    Ok(0)
}

async fn handle_attach_runtime_action(
    client: &mut BmuxClient,
    action: RuntimeAction,
    attached_id: &mut Uuid,
    can_write: &mut bool,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::NewWindow => {
            let window_id = client
                .new_window(Some(SessionSelector::ById(*attached_id)), None)
                .await?;
            let active_window_id = client
                .switch_window(
                    Some(SessionSelector::ById(*attached_id)),
                    WindowSelector::ById(window_id),
                )
                .await?;
            println!("created window: {window_id}");
            println!("switched to window: {active_window_id}");
        }
        RuntimeAction::NewSession => {
            let session_id = client.new_session(None).await?;
            let attach_info = open_attach_for_session(client, session_id).await?;
            *attached_id = attach_info.session_id;
            *can_write = attach_info.can_write;
            println!(
                "created and switched to session: {}",
                attach_info.session_id
            );
            if !*can_write {
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
    attached_id: &mut Uuid,
    can_write: &mut bool,
    ui_mode: &mut AttachUiMode,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::EnterWindowMode => {
            *ui_mode = AttachUiMode::Window;
        }
        RuntimeAction::ExitMode => {
            *ui_mode = AttachUiMode::Normal;
        }
        RuntimeAction::WindowPrev => {
            switch_attach_window_relative(client, *attached_id, -1).await?;
        }
        RuntimeAction::WindowNext => {
            switch_attach_window_relative(client, *attached_id, 1).await?;
        }
        RuntimeAction::WindowGoto1 => switch_attach_window_index(client, *attached_id, 0).await?,
        RuntimeAction::WindowGoto2 => switch_attach_window_index(client, *attached_id, 1).await?,
        RuntimeAction::WindowGoto3 => switch_attach_window_index(client, *attached_id, 2).await?,
        RuntimeAction::WindowGoto4 => switch_attach_window_index(client, *attached_id, 3).await?,
        RuntimeAction::WindowGoto5 => switch_attach_window_index(client, *attached_id, 4).await?,
        RuntimeAction::WindowGoto6 => switch_attach_window_index(client, *attached_id, 5).await?,
        RuntimeAction::WindowGoto7 => switch_attach_window_index(client, *attached_id, 6).await?,
        RuntimeAction::WindowGoto8 => switch_attach_window_index(client, *attached_id, 7).await?,
        RuntimeAction::WindowGoto9 => switch_attach_window_index(client, *attached_id, 8).await?,
        RuntimeAction::WindowClose => {
            let _ = client
                .kill_window(
                    Some(SessionSelector::ById(*attached_id)),
                    WindowSelector::Active,
                )
                .await?;
        }
        RuntimeAction::SplitFocusedVertical => {
            let _ = client
                .split_pane(
                    Some(SessionSelector::ById(*attached_id)),
                    PaneSplitDirection::Vertical,
                )
                .await?;
        }
        RuntimeAction::SplitFocusedHorizontal => {
            let _ = client
                .split_pane(
                    Some(SessionSelector::ById(*attached_id)),
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
                .focus_pane(Some(SessionSelector::ById(*attached_id)), direction)
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
                .resize_pane(Some(SessionSelector::ById(*attached_id)), delta)
                .await?;
        }
        RuntimeAction::CloseFocusedPane => {
            client
                .close_pane(Some(SessionSelector::ById(*attached_id)))
                .await?;
        }
        RuntimeAction::NewWindow | RuntimeAction::NewSession => {
            handle_attach_runtime_action(client, action, attached_id, can_write).await?;
        }
        _ => {}
    }

    Ok(())
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

async fn switch_attach_window_index(
    client: &mut BmuxClient,
    session_id: Uuid,
    target_index: usize,
) -> std::result::Result<(), ClientError> {
    let windows = ordered_session_windows(client, session_id).await?;
    let Some(target) = windows.get(target_index) else {
        return Ok(());
    };

    let _ = client
        .switch_window(
            Some(SessionSelector::ById(session_id)),
            WindowSelector::ById(target.id),
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

fn window_sort_rank(window: &bmux_ipc::WindowSummary) -> (u8, u32, String) {
    if let Some(index) = window.name.as_deref().and_then(parse_window_auto_index) {
        return (0, index, String::new());
    }

    let normalized = window
        .name
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    (1, u32::MAX, normalized)
}

fn parse_window_auto_index(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix("window-")?;
    suffix.parse::<u32>().ok()
}

async fn build_attach_status_line_for_draw(
    client: &mut BmuxClient,
    session_id: Uuid,
    can_write: bool,
    ui_mode: AttachUiMode,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    quit_confirmation_pending: bool,
    keymap: &Keymap,
) -> std::result::Result<String, ClientError> {
    let (cols, _) = terminal::size().unwrap_or((0, 0));
    if cols == 0 {
        return Ok(String::new());
    }

    let tabs = build_attach_tabs(client, session_id).await?;
    let session_label = resolve_attach_session_label(client, session_id).await?;
    let mode_label = match ui_mode {
        AttachUiMode::Normal => "NORMAL",
        AttachUiMode::Window => "WINDOW",
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
    } else {
        attach_mode_hint(ui_mode, keymap)
    };

    let status_line = build_attach_status_line(
        &session_label,
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
            format!("{window_mode} window mode | {detach} detach | {quit} quit")
        }
        AttachUiMode::Window => {
            let prev = key_hint_or_unbound(keymap, RuntimeAction::WindowPrev);
            let next = key_hint_or_unbound(keymap, RuntimeAction::WindowNext);
            let goto_one = key_hint_or_unbound(keymap, RuntimeAction::WindowGoto1);
            let new_window = key_hint_or_unbound(keymap, RuntimeAction::NewWindow);
            let close = key_hint_or_unbound(keymap, RuntimeAction::WindowClose);
            let exit = key_hint_or_unbound(keymap, RuntimeAction::ExitMode);
            format!(
                "{prev}/{next} prev/next | {goto_one} goto-1 | {new_window} new | {close} close | {exit} exit"
            )
        }
    }
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

async fn render_attach_frame(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    layout_state: &AttachLayoutState,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    keymap: &crate::input::Keymap,
) -> Result<()> {
    if view_state.dirty.status_needs_redraw {
        view_state.cached_status_line = Some(
            build_attach_status_line_for_draw(
                client,
                view_state.attached_id,
                view_state.can_write,
                view_state.ui_mode,
                follow_target_id,
                follow_global,
                view_state.quit_confirmation_pending,
                keymap,
            )
            .await
            .map_err(map_attach_client_error)?,
        );
        view_state.dirty.status_needs_redraw = false;
    }

    let mut stdout = io::stdout();
    queue!(stdout, Hide).context("failed queuing hide cursor for attach frame")?;
    if let Some(status_line) = view_state.cached_status_line.as_deref() {
        queue_attach_status_line(&mut stdout, status_line)?;
    }
    let cursor_state = render_attach_panes(
        &mut stdout,
        &layout_state.layout_root,
        layout_state.focused_pane_id,
        &mut view_state.pane_buffers,
    )?;
    apply_attach_cursor_state(&mut stdout, cursor_state)?;
    stdout.flush().context("failed flushing attach frame")
}

async fn build_attach_tabs(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<Vec<AttachTab>, ClientError> {
    let windows = ordered_session_windows(client, session_id).await?;
    Ok(windows
        .into_iter()
        .enumerate()
        .map(|(index, window)| AttachTab {
            index: index + 1,
            title: window
                .name
                .unwrap_or_else(|| format!("window-{}", short_uuid(window.id))),
            active: window.active,
        })
        .collect())
}

async fn resolve_attach_session_label(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let sessions = client.list_sessions().await?;
    Ok(sessions
        .into_iter()
        .find(|session| session.id == session_id)
        .map(|session| {
            session
                .name
                .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
        })
        .unwrap_or_else(|| format!("session-{}", short_uuid(session_id))))
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
        .ok_or_else(|| ClientError::UnexpectedResponse("follow target has no selected session"))
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
    match crate::input::Keymap::from_parts(
        &config.keybindings.prefix,
        config.keybindings.timeout_ms,
        &runtime_bindings,
        &global_bindings,
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

fn normalize_attach_keybindings(
    bindings: std::collections::BTreeMap<String, String>,
    scope: &str,
) -> std::collections::BTreeMap<String, String> {
    bindings
        .into_iter()
        .filter_map(
            |(chord, action_name)| match crate::input::parse_runtime_action_name(&action_name) {
                Ok(action) if is_attach_runtime_action(&action) => {
                    Some((chord, crate::input::action_to_name(&action).to_string()))
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
        ("ctrl+t", "enter_window_mode"),
        ("escape", "exit_mode"),
        ("enter", "exit_mode"),
        ("h", "window_prev"),
        ("l", "window_next"),
        ("1", "window_goto_1"),
        ("2", "window_goto_2"),
        ("3", "window_goto_3"),
        ("4", "window_goto_4"),
        ("5", "window_goto_5"),
        ("6", "window_goto_6"),
        ("7", "window_goto_7"),
        ("8", "window_goto_8"),
        ("9", "window_goto_9"),
        ("x", "window_close"),
        ("n", "new_window"),
    ];

    for (key, action) in defaults {
        global
            .entry(key.to_string())
            .or_insert_with(|| action.to_string());
    }
}

fn is_attach_runtime_action(action: &RuntimeAction) -> bool {
    matches!(
        action,
        RuntimeAction::Detach
            | RuntimeAction::Quit
            | RuntimeAction::NewWindow
            | RuntimeAction::NewSession
            | RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
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
    )
}

fn default_attach_keymap() -> crate::input::Keymap {
    let defaults = BmuxConfig::default();
    let (runtime_bindings, global_bindings) = filtered_attach_keybindings(&defaults);
    crate::input::Keymap::from_parts(
        &defaults.keybindings.prefix,
        defaults.keybindings.timeout_ms,
        &runtime_bindings,
        &global_bindings,
    )
    .expect("default attach keymap must be valid")
}

enum AttachEventAction {
    Send(Vec<u8>),
    Runtime(RuntimeAction),
    Ui(RuntimeAction),
    Redraw,
    Detach,
    Ignore,
}

enum AttachLoopEvent {
    Server(bmux_client::ServerEvent),
    Terminal(Event),
}

enum AttachLoopControl {
    Continue,
    Break(AttachExitReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachUiMode {
    Normal,
    Window,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachExitReason {
    Detached,
    StreamClosed,
    Quit,
}

#[derive(Debug, Clone, Copy)]
struct AttachDirtyFlags {
    status_needs_redraw: bool,
    layout_needs_refresh: bool,
}

impl Default for AttachDirtyFlags {
    fn default() -> Self {
        Self {
            status_needs_redraw: true,
            layout_needs_refresh: true,
        }
    }
}

struct AttachViewState {
    attached_id: Uuid,
    can_write: bool,
    ui_mode: AttachUiMode,
    quit_confirmation_pending: bool,
    pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    cached_status_line: Option<String>,
    cached_layout_state: Option<AttachLayoutState>,
    dirty: AttachDirtyFlags,
}

impl AttachViewState {
    fn new(attach_info: bmux_client::AttachOpenInfo) -> Self {
        Self {
            attached_id: attach_info.session_id,
            can_write: attach_info.can_write,
            ui_mode: AttachUiMode::Normal,
            quit_confirmation_pending: false,
            pane_buffers: BTreeMap::new(),
            cached_status_line: None,
            cached_layout_state: None,
            dirty: AttachDirtyFlags::default(),
        }
    }
}

#[derive(Clone, Copy)]
struct PaneRect {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct AttachCursorState {
    x: u16,
    y: u16,
    visible: bool,
}

struct PaneRenderBuffer {
    parser: vt100::Parser,
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, 4_096),
        }
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

fn collect_attach_loop_events(
    server_events: Vec<bmux_client::ServerEvent>,
    terminal_event: Option<Event>,
) -> Vec<AttachLoopEvent> {
    let mut events = server_events
        .into_iter()
        .map(AttachLoopEvent::Server)
        .collect::<Vec<_>>();
    if let Some(event) = terminal_event {
        events.push(AttachLoopEvent::Terminal(event));
    }
    events
}

async fn handle_attach_loop_event(
    event: AttachLoopEvent,
    client: &mut BmuxClient,
    attach_input_processor: &mut InputProcessor,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    global: bool,
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    match event {
        AttachLoopEvent::Server(server_event) => handle_attach_server_event(
            client,
            server_event,
            follow_target_id,
            self_client_id,
            global,
            view_state,
        )
        .await,
        AttachLoopEvent::Terminal(terminal_event) => {
            handle_attach_terminal_event(client, terminal_event, attach_input_processor, view_state)
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
    match server_event {
        bmux_client::ServerEvent::SessionRemoved { id } if id == view_state.attached_id => {
            return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
        }
        bmux_client::ServerEvent::ClientDetached { id } if id == view_state.attached_id => {
            return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
        }
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
            view_state.ui_mode = AttachUiMode::Normal;
            view_state.dirty.status_needs_redraw = true;
            view_state.dirty.layout_needs_refresh = true;
            println!("follow handoff -> session {}", view_state.attached_id);
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
        _ => {}
    }

    Ok(AttachLoopControl::Continue)
}

async fn handle_attach_terminal_event(
    client: &mut BmuxClient,
    terminal_event: Event,
    attach_input_processor: &mut InputProcessor,
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    let mut skip_attach_key_actions = false;
    if view_state.quit_confirmation_pending {
        if let Event::Key(key) = &terminal_event
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    match client
                        .kill_session(SessionSelector::ById(view_state.attached_id))
                        .await
                    {
                        Ok(_) => return Ok(AttachLoopControl::Break(AttachExitReason::Quit)),
                        Err(error) => {
                            println!("quit failed: {}", map_attach_client_error(error));
                        }
                    }
                    view_state.quit_confirmation_pending = false;
                    view_state.dirty.status_needs_redraw = true;
                    skip_attach_key_actions = true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                    view_state.quit_confirmation_pending = false;
                    view_state.dirty.status_needs_redraw = true;
                    skip_attach_key_actions = true;
                }
                _ => {
                    skip_attach_key_actions = true;
                }
            }
        }
    }

    if skip_attach_key_actions {
        return Ok(AttachLoopControl::Continue);
    }

    for attach_action in attach_event_actions(
        &terminal_event,
        attach_input_processor,
        view_state.ui_mode,
    )? {
        match attach_action {
            AttachEventAction::Detach => return Ok(AttachLoopControl::Break(AttachExitReason::Detached)),
            AttachEventAction::Send(bytes) => {
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
                if let Err(error) = handle_attach_runtime_action(
                    client,
                    action,
                    &mut view_state.attached_id,
                    &mut view_state.can_write,
                )
                .await
                {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.layout_needs_refresh = true;
                }
            }
            AttachEventAction::Ui(action) => {
                if matches!(action, RuntimeAction::Quit) {
                    view_state.quit_confirmation_pending = true;
                    view_state.dirty.status_needs_redraw = true;
                    continue;
                }
                if let Err(error) = handle_attach_ui_action(
                    client,
                    action,
                    &mut view_state.attached_id,
                    &mut view_state.can_write,
                    &mut view_state.ui_mode,
                )
                .await
                {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.layout_needs_refresh = true;
                }
                view_state.dirty.status_needs_redraw = true;
            }
            AttachEventAction::Redraw => {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
            }
            AttachEventAction::Ignore => {}
        }
    }

    Ok(AttachLoopControl::Continue)
}

fn collect_pane_ids(layout: &PaneLayoutNode, out: &mut Vec<Uuid>) {
    match layout {
        PaneLayoutNode::Leaf { pane_id } => out.push(*pane_id),
        PaneLayoutNode::Split { first, second, .. } => {
            collect_pane_ids(first, out);
            collect_pane_ids(second, out);
        }
    }
}

fn split_rect(rect: PaneRect, ratio_percent: u8, vertical: bool) -> (PaneRect, PaneRect) {
    if vertical {
        let split = ((u32::from(rect.w) * u32::from(ratio_percent)) / 100) as u16;
        let left_w = split.max(1).min(rect.w.saturating_sub(1));
        let right_w = rect.w.saturating_sub(left_w);
        (
            PaneRect {
                x: rect.x,
                y: rect.y,
                w: left_w,
                h: rect.h,
            },
            PaneRect {
                x: rect.x.saturating_add(left_w),
                y: rect.y,
                w: right_w,
                h: rect.h,
            },
        )
    } else {
        let split = ((u32::from(rect.h) * u32::from(ratio_percent)) / 100) as u16;
        let top_h = split.max(1).min(rect.h.saturating_sub(1));
        let bottom_h = rect.h.saturating_sub(top_h);
        (
            PaneRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: top_h,
            },
            PaneRect {
                x: rect.x,
                y: rect.y.saturating_add(top_h),
                w: rect.w,
                h: bottom_h,
            },
        )
    }
}

fn collect_layout_rects(
    layout: &PaneLayoutNode,
    rect: PaneRect,
    out: &mut BTreeMap<Uuid, PaneRect>,
) {
    match layout {
        PaneLayoutNode::Leaf { pane_id } => {
            out.insert(*pane_id, rect);
        }
        PaneLayoutNode::Split {
            direction,
            ratio_percent,
            first,
            second,
        } => {
            let vertical = matches!(direction, PaneSplitDirection::Vertical);
            let (first_rect, second_rect) = split_rect(rect, *ratio_percent, vertical);
            collect_layout_rects(first, first_rect, out);
            collect_layout_rects(second, second_rect, out);
        }
    }
}

fn append_pane_output(buffer: &mut PaneRenderBuffer, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    buffer.parser.process(bytes);
}

fn draw_box_line(width: usize, left: char, mid: char, right: char) -> String {
    if width <= 1 {
        return left.to_string();
    }
    let mut line = String::new();
    line.push(left);
    if width > 2 {
        line.extend(std::iter::repeat_n(mid, width - 2));
    }
    line.push(right);
    line
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct CellStyle {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

fn cell_style(cell: &vt100::Cell) -> CellStyle {
    CellStyle {
        fg: cell.fgcolor(),
        bg: cell.bgcolor(),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        inverse: cell.inverse(),
    }
}

fn color_sgr(color: vt100::Color, foreground: bool) -> String {
    match color {
        vt100::Color::Default => {
            if foreground {
                "39".to_string()
            } else {
                "49".to_string()
            }
        }
        vt100::Color::Idx(idx) => {
            if foreground {
                format!("38;5;{idx}")
            } else {
                format!("48;5;{idx}")
            }
        }
        vt100::Color::Rgb(r, g, b) => {
            if foreground {
                format!("38;2;{r};{g};{b}")
            } else {
                format!("48;2;{r};{g};{b}")
            }
        }
    }
}

fn style_sgr(style: CellStyle) -> String {
    let mut parts = vec!["0".to_string()];
    if style.bold {
        parts.push("1".to_string());
    }
    if style.dim {
        parts.push("2".to_string());
    }
    if style.italic {
        parts.push("3".to_string());
    }
    if style.underline {
        parts.push("4".to_string());
    }
    if style.inverse {
        parts.push("7".to_string());
    }
    parts.push(color_sgr(style.fg, true));
    parts.push(color_sgr(style.bg, false));
    format!("\x1b[{}m", parts.join(";"))
}

fn render_attach_panes(
    stdout: &mut io::Stdout,
    layout: &PaneLayoutNode,
    focused_pane_id: Uuid,
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
) -> Result<Option<AttachCursorState>> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows <= 1 {
        return Ok(None);
    }

    let draw_rows = rows.saturating_sub(1);
    let root = PaneRect {
        x: 0,
        y: 1,
        w: cols,
        h: draw_rows,
    };

    let mut rects = BTreeMap::new();
    collect_layout_rects(layout, root, &mut rects);

    let mut cursor_state = None;
    for y in 1..rows {
        queue!(stdout, MoveTo(0, y), Print(" ".repeat(usize::from(cols))))
            .context("failed clearing attach pane row")?;
    }

    for (pane_id, rect) in rects {
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let focus = pane_id == focused_pane_id;
        let hch = if focus { '=' } else { '-' };
        let top = draw_box_line(usize::from(rect.w), '+', hch, '+');
        let bottom = draw_box_line(usize::from(rect.w), '+', hch, '+');
        queue!(stdout, MoveTo(rect.x, rect.y), Print(top)).context("failed drawing pane top")?;
        queue!(
            stdout,
            MoveTo(rect.x, rect.y.saturating_add(rect.h.saturating_sub(1))),
            Print(bottom)
        )
        .context("failed drawing pane bottom")?;

        for y in rect.y.saturating_add(1)..rect.y.saturating_add(rect.h.saturating_sub(1)) {
            queue!(stdout, MoveTo(rect.x, y), Print("|"))
                .context("failed drawing pane left border")?;
            queue!(
                stdout,
                MoveTo(rect.x.saturating_add(rect.w.saturating_sub(1)), y),
                Print("|")
            )
            .context("failed drawing pane right border")?;
        }

        let inner_w_u16 = rect.w.saturating_sub(2);
        let inner_h_u16 = rect.h.saturating_sub(2);
        let inner_w = usize::from(inner_w_u16);
        let inner_h = usize::from(inner_h_u16);
        if let Some(entry) = pane_buffers.get_mut(&pane_id) {
            entry
                .parser
                .screen_mut()
                .set_size(inner_h_u16.max(1), inner_w_u16.max(1));
            let screen = entry.parser.screen();
            if pane_id == focused_pane_id {
                let (cursor_row, cursor_col) = screen.cursor_position();
                let cursor_row = cursor_row.min(inner_h_u16.saturating_sub(1));
                let cursor_col = cursor_col.min(inner_w_u16.saturating_sub(1));
                cursor_state = Some(AttachCursorState {
                    x: rect.x.saturating_add(1).saturating_add(cursor_col),
                    y: rect.y.saturating_add(1).saturating_add(cursor_row),
                    visible: !screen.hide_cursor(),
                });
            }
            for row in 0..inner_h {
                let y = rect.y.saturating_add(1 + row as u16);
                let mut line = String::new();
                let mut current = CellStyle::default();
                let mut used_cols = 0usize;
                let mut col = 0u16;
                while col < inner_w_u16 {
                    if let Some(cell) = screen.cell(row as u16, col) {
                        let style = cell_style(cell);
                        if style != current {
                            line.push_str(&style_sgr(style));
                            current = style;
                        }
                        if cell.is_wide_continuation() {
                            line.push(' ');
                            used_cols = used_cols.saturating_add(1);
                            col = col.saturating_add(1);
                            continue;
                        }
                        let text = if cell.has_contents() {
                            cell.contents()
                        } else {
                            " "
                        };
                        line.push_str(text);
                        let width = UnicodeWidthStr::width(text).max(1);
                        used_cols = used_cols.saturating_add(width);
                        if cell.is_wide() {
                            col = col.saturating_add(2);
                        } else {
                            col = col.saturating_add(1);
                        }
                    } else {
                        if current != CellStyle::default() {
                            line.push_str("\x1b[0m");
                            current = CellStyle::default();
                        }
                        line.push(' ');
                        used_cols = used_cols.saturating_add(1);
                        col = col.saturating_add(1);
                    }
                }

                if used_cols < inner_w {
                    if current != CellStyle::default() {
                        line.push_str("\x1b[0m");
                    }
                    line.push_str(&" ".repeat(inner_w - used_cols));
                } else if current != CellStyle::default() {
                    line.push_str("\x1b[0m");
                }

                queue!(stdout, MoveTo(rect.x.saturating_add(1), y), Print(line))
                    .context("failed drawing pane content")?;
            }
        } else {
            for row in 0..inner_h {
                let y = rect.y.saturating_add(1 + row as u16);
                queue!(
                    stdout,
                    MoveTo(rect.x.saturating_add(1), y),
                    Print(" ".repeat(inner_w))
                )
                .context("failed clearing pane content")?;
            }
        }
    }

    Ok(cursor_state)
}

fn apply_attach_cursor_state(
    stdout: &mut io::Stdout,
    cursor_state: Option<AttachCursorState>,
) -> Result<()> {
    match cursor_state {
        Some(state) if state.visible => {
            queue!(stdout, MoveTo(state.x, state.y), Show)
                .context("failed applying visible attach cursor")?;
        }
        _ => {
            queue!(stdout, Hide).context("failed applying hidden attach cursor")?;
        }
    }
    Ok(())
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

    let actions = attach_input_processor.process_terminal_event(Event::Key(key.clone()));
    Ok(actions
        .into_iter()
        .map(|action| match action {
            RuntimeAction::Detach => AttachEventAction::Detach,
            RuntimeAction::ForwardToPane(bytes) => {
                if ui_mode == AttachUiMode::Window {
                    AttachEventAction::Ignore
                } else {
                    AttachEventAction::Send(bytes)
                }
            }
            RuntimeAction::NewWindow | RuntimeAction::NewSession => {
                AttachEventAction::Runtime(action)
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
            | RuntimeAction::CloseFocusedPane => AttachEventAction::Ui(action),
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
                        .map(AttachEventAction::Send)
                        .unwrap_or(AttachEventAction::Ignore)
                }
            }
            RuntimeAction::Quit => AttachEventAction::Ui(action),
            RuntimeAction::ToggleSplitDirection
            | RuntimeAction::RestartFocusedPane
            | RuntimeAction::ShowHelp
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
            | RuntimeAction::CopyScrollback => AttachEventAction::Ignore,
        })
        .collect())
}

fn is_attach_stream_closed_error(error: &ClientError) -> bool {
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
        other => map_cli_client_error(other),
    }
}

fn map_cli_client_error(error: ClientError) -> anyhow::Error {
    if let ClientError::Transport(IpcTransportError::Io(io_error)) = &error
        && io_error.kind() == std::io::ErrorKind::NotFound
    {
        return anyhow::anyhow!(
            "bmux server is not running (IPC socket not found).\nRun `bmux server start --daemon`.\nTroubleshooting: if the server is running in another shell, ensure both processes use the same runtime directory (`XDG_RUNTIME_DIR`/`TMPDIR`)."
        );
    }

    anyhow::Error::from(error)
}

async fn run_session_detach() -> Result<u8> {
    let mut client = BmuxClient::connect_default("bmux-cli-detach")
        .await
        .map_err(map_cli_client_error)?;
    client.detach().await.map_err(map_cli_client_error)?;
    println!("detached");
    Ok(0)
}

async fn run_follow(target_client_id: &str, global: bool) -> Result<u8> {
    let target_client_id = parse_uuid_value(target_client_id, "target client id")?;
    let mut client = BmuxClient::connect_default("bmux-cli-follow")
        .await
        .map_err(map_cli_client_error)?;
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
    let mut client = BmuxClient::connect_default("bmux-cli-unfollow")
        .await
        .map_err(map_cli_client_error)?;
    client.unfollow().await.map_err(map_cli_client_error)?;
    println!("follow stopped");
    Ok(0)
}

async fn run_window_new(session: Option<&String>, name: Option<String>) -> Result<u8> {
    let session_selector = session.map(|target| parse_session_selector(target));
    let mut client = BmuxClient::connect_default("bmux-cli-new-window")
        .await
        .map_err(map_cli_client_error)?;
    let window_id = client
        .new_window(session_selector, name)
        .await
        .map_err(map_cli_client_error)?;
    println!("created window: {window_id}");
    Ok(0)
}

async fn run_window_list(session: Option<&String>, as_json: bool) -> Result<u8> {
    let session_selector = session.map(|target| parse_session_selector(target));
    let mut client = BmuxClient::connect_default("bmux-cli-list-windows")
        .await
        .map_err(map_cli_client_error)?;
    let windows = client
        .list_windows(session_selector)
        .await
        .map_err(map_cli_client_error)?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&windows).context("failed to encode windows json")?
        );
        return Ok(0);
    }

    if windows.is_empty() {
        println!("no windows");
        return Ok(0);
    }

    println!(
        "ID                                   SESSION                              NAME            ACTIVE"
    );
    for window in windows {
        let name = window.name.unwrap_or_else(|| "-".to_string());
        println!(
            "{:<36} {:<36} {:<15} {}",
            window.id,
            window.session_id,
            name,
            if window.active { "yes" } else { "no" }
        );
    }

    Ok(0)
}

async fn run_window_kill(target: &str, session: Option<&String>, force_local: bool) -> Result<u8> {
    let session_selector = session.map(|value| parse_session_selector(value));
    let window_selector = parse_window_selector(target);
    let mut client = BmuxClient::connect_default("bmux-cli-kill-window")
        .await
        .map_err(map_cli_client_error)?;
    let window_id = client
        .kill_window_with_options(session_selector, window_selector, force_local)
        .await
        .map_err(map_cli_client_error)?;
    println!("killed window: {window_id}");
    Ok(0)
}

async fn run_window_kill_all(session: Option<&String>, force_local: bool) -> Result<u8> {
    let session_selector = session.map(|value| parse_session_selector(value));
    let mut client = BmuxClient::connect_default("bmux-cli-kill-all-windows")
        .await
        .map_err(map_cli_client_error)?;
    let windows = client
        .list_windows(session_selector.clone())
        .await
        .map_err(map_cli_client_error)?;

    if windows.is_empty() {
        println!("no windows");
        return Ok(0);
    }

    let mut killed_count = 0usize;
    let mut failed_count = 0usize;
    for window in windows {
        match client
            .kill_window_with_options(
                session_selector.clone(),
                WindowSelector::ById(window.id),
                force_local,
            )
            .await
        {
            Ok(window_id) => {
                println!("killed window: {window_id}");
                killed_count = killed_count.saturating_add(1);
            }
            Err(error) => {
                failed_count = failed_count.saturating_add(1);
                let mapped_error = map_cli_client_error(error);
                eprintln!("failed killing window {}: {mapped_error:#}", window.id);
            }
        }
    }

    println!("kill-all-windows complete: killed {killed_count}, failed {failed_count}");
    Ok(if failed_count == 0 { 0 } else { 1 })
}

async fn run_window_switch(target: &str, session: Option<&String>) -> Result<u8> {
    let session_selector = session.map(|value| parse_session_selector(value));
    let window_selector = parse_window_selector(target);
    let mut client = BmuxClient::connect_default("bmux-cli-switch-window")
        .await
        .map_err(map_cli_client_error)?;
    let window_id = client
        .switch_window(session_selector, window_selector)
        .await
        .map_err(map_cli_client_error)?;
    println!("active window: {window_id}");
    Ok(0)
}

fn parse_session_selector(target: &str) -> SessionSelector {
    match Uuid::parse_str(target) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(target.to_string()),
    }
}

fn parse_window_selector(target: &str) -> WindowSelector {
    if target.eq_ignore_ascii_case("active") {
        return WindowSelector::Active;
    }

    match Uuid::parse_str(target) {
        Ok(id) => WindowSelector::ById(id),
        Err(_) => WindowSelector::ByName(target.to_string()),
    }
}

fn parse_uuid_value(value: &str, label: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("{label} must be a UUID, got '{value}'"))
}

fn session_role_from_value(role: RoleValue) -> SessionRole {
    match role {
        RoleValue::Owner => SessionRole::Owner,
        RoleValue::Writer => SessionRole::Writer,
        RoleValue::Observer => SessionRole::Observer,
    }
}

fn session_role_label(role: SessionRole) -> &'static str {
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
    let connect = tokio::time::timeout(
        SERVER_STATUS_TIMEOUT,
        BmuxClient::connect_default("bmux-cli-status"),
    )
    .await;

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
        let connect = tokio::time::timeout(
            SERVER_STATUS_TIMEOUT,
            BmuxClient::connect_default("bmux-cli-start-wait"),
        )
        .await;
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
        let reconnect = tokio::time::timeout(
            SERVER_STATUS_TIMEOUT,
            BmuxClient::connect_default("bmux-cli-stop-check"),
        )
        .await;
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

fn server_runtime_metadata_file_path() -> PathBuf {
    let paths = bmux_config::ConfigPaths::default();
    paths.runtime_dir.join("server-meta.json")
}

fn current_cli_build_id() -> Result<String> {
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
    })
}

fn write_server_runtime_metadata(pid: u32) -> Result<()> {
    let path = server_runtime_metadata_file_path();
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

fn read_server_runtime_metadata() -> Result<Option<ServerRuntimeMetadata>> {
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

fn remove_server_runtime_metadata_file() -> Result<()> {
    let path = server_runtime_metadata_file_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed removing server metadata file {}", path.display())),
    }
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

    match parse_pid_content(&content) {
        Some(pid) => Ok(Some(pid)),
        None => {
            let _ = remove_server_pid_file();
            Ok(None)
        }
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
        return Ok(status.success());
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
        return Ok(status.success());
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
    let configured = BmuxConfig::load()
        .map(|cfg| cfg.behavior.pane_term)
        .unwrap_or_else(|_| "bmux-256color".to_string());
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
        println!("trace events (latest {}):", trace_limit);
        println!("trace dropped events: {}", trace_data.dropped);
        if trace_family.is_some() || trace_pane.is_some() {
            println!(
                "trace filters: family={} pane={}",
                trace_family.map(trace_family_name).unwrap_or("any"),
                trace_pane
                    .map(|pane| pane.to_string())
                    .unwrap_or_else(|| "any".to_string())
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
            let family_matches = family
                .map(|value| event.family == trace_family_name(value))
                .unwrap_or(true);
            let pane_matches = pane
                .map(|value| event.pane_id == Some(value))
                .unwrap_or(true);
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

fn trace_family_name(family: TraceFamily) -> &'static str {
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

fn terminfo_auto_install_name(policy: TerminfoAutoInstall) -> &'static str {
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
    let mut pane_term = configured_normalized.clone();

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
            "could not verify terminfo for pane TERM '{}'; continuing without fallback checks",
            pane_term
        ));
    }

    let profile = profile_for_term(&pane_term);

    let effective_terminfo_available = terminfo_checks
        .iter()
        .find_map(|(term, available)| (term == &pane_term).then_some(*available))
        .flatten();

    if profile == TerminalProfile::Conservative {
        warnings.push(format!(
            "pane TERM '{}' uses conservative capability profile; compatibility depends on host terminfo",
            pane_term
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

fn terminal_profile_name(profile: TerminalProfile) -> &'static str {
    match profile {
        TerminalProfile::Bmux256Color => "bmux-256color",
        TerminalProfile::Screen256Color => "screen-256color-compatible",
        TerminalProfile::Xterm256Color => "xterm-256color-compatible",
        TerminalProfile::Conservative => "conservative",
    }
}

fn protocol_profile_for_terminal_profile(profile: TerminalProfile) -> ProtocolProfile {
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
    let keymap = crate::input::Keymap::from_parts(
        &config.keybindings.prefix,
        config.keybindings.timeout_ms,
        &runtime_bindings,
        &global_bindings,
    )
    .context("failed to compile keymap")?;

    let report = keymap.doctor_report();

    if as_json {
        let payload = serde_json::json!({
            "prefix": config.keybindings.prefix,
            "timeout_ms": config.keybindings.timeout_ms,
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
    println!("timeout_ms: {}", config.keybindings.timeout_ms);
    for line in keymap.doctor_lines() {
        println!("{line}");
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
        attach_keymap_from_config, filter_trace_events, map_attach_client_error,
        map_cli_client_error, merged_runtime_keybindings, parse_pid_content, profile_for_term,
        protocol_profile_for_terminal_profile, resolve_pane_term_with_checker,
    };
    use crate::input::InputProcessor;
    use bmux_client::ClientError;
    use bmux_config::BmuxConfig;
    use bmux_ipc::ErrorCode;
    use bmux_ipc::transport::IpcTransportError;
    use crossterm::event::{
        KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers,
    };

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
            .insert("h".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("l".to_string(), "new_session".to_string());
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
        assert!(hint.contains("u/i prev/next"));
        assert!(hint.contains("0 goto-1"));
        assert!(hint.contains("m new"));
        assert!(hint.contains("k close"));
        assert!(hint.contains("Ctrl-G exit"));
    }

    #[test]
    fn attach_keybindings_keep_focus_next_pane_binding() {
        let (runtime, _global) = super::filtered_attach_keybindings(&BmuxConfig::default());
        assert_eq!(runtime.get("o"), Some(&"focus_next_pane".to_string()));
    }

    #[test]
    fn sort_attach_windows_prefers_window_number_then_name() {
        let mut windows = vec![
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000003")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                name: Some("editor".to_string()),
                active: false,
            },
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                name: Some("window-10".to_string()),
                active: false,
            },
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000002")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                name: Some("window-2".to_string()),
                active: true,
            },
            bmux_ipc::WindowSummary {
                id: uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000004")
                    .expect("valid uuid"),
                session_id: uuid::Uuid::nil(),
                name: Some("zeta".to_string()),
                active: false,
            },
        ];

        super::sort_attach_windows(&mut windows);

        let ordered_names: Vec<String> = windows
            .into_iter()
            .map(|window| window.name.unwrap_or_default())
            .collect();
        assert_eq!(
            ordered_names,
            vec!["window-2", "window-10", "editor", "zeta"]
        );
    }
}
