use crate::cli::{
    Cli, Command, DebugRenderLogFormat, KeymapCommand, LayoutCommand, ServerCommand,
    TerminalCommand, TraceFamily,
};
use crate::input::{InputProcessor, RuntimeAction};
use crate::pane::{LayoutTree, PaneId, SplitDirection};
use crate::pty::STARTUP_ALT_SCREEN_GUARD_DURATION;
use crate::terminal::TerminalGuard;
use anyhow::{Context, Result};
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, TerminfoAutoInstall};
use bmux_server::BmuxServer;
use clap::Parser;
use crossterm::event::{self, Event};
use portable_pty::{Child, MasterPty};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::debug;
use vt100::Parser as VtParser;

mod commands;
mod compositor;
mod pane_runtime;
mod persistence;
mod status_message;
mod terminal_protocol;
use commands::process_input_events;
use compositor::{RenderCache, RenderDebugState, SelectionOverlay, render_frame};
use pane_runtime::{
    any_running_panes, first_running_pane_id, pane_is_running, refresh_exit_codes, resize_panes,
    spawn_pane, stop_pane_process,
};
use persistence::{load_persisted_runtime_state, save_persisted_runtime_state};
use status_message::StatusMessage;
use terminal_protocol::{
    ProtocolDirection, ProtocolProfile, ProtocolTraceBuffer, ProtocolTraceEvent,
    SharedProtocolTraceBuffer, primary_da_for_profile, protocol_profile_name,
    secondary_da_for_profile, supported_query_names,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STATUS_REDRAW_INTERVAL: Duration = Duration::from_millis(200);
const SERVER_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_millis(5000);
const MIN_PANE_ROWS: u16 = 2;
const MIN_PANE_COLS: u16 = 2;

struct PaneState {
    parser: Mutex<VtParser>,
    dirty: AtomicBool,
}

struct PaneRuntime {
    title: String,
    shell: String,
    state: Arc<PaneState>,
    process: Option<PaneProcess>,
    closed: bool,
    exit_code: Option<u8>,
}

struct PaneProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send>,
    output_thread: Option<thread::JoinHandle<Result<()>>>,
}

struct ReapExitedPanesResult {
    removed_any: bool,
    session_exit_code: Option<u8>,
}

struct RuntimeSettings {
    keymap: crate::input::Keymap,
    layout_persistence_enabled: bool,
    scrollback_limit: usize,
    pane_term: String,
    terminal_profile: TerminalProfile,
    protocol_profile: ProtocolProfile,
    protocol_trace_enabled: bool,
    protocol_trace_capacity: usize,
    configured_pane_term: String,
    warnings: Vec<String>,
}

#[derive(Default)]
struct ScrollState {
    active: bool,
    offsets: BTreeMap<PaneId, usize>,
    cursors: BTreeMap<PaneId, (u16, u16)>,
    selection_anchors: BTreeMap<PaneId, (u16, u16)>,
}

struct RuntimeOptions {
    terminfo_auto_install: TerminfoAutoInstall,
    terminfo_prompt_cooldown_days: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProfile {
    Bmux256Color,
    Screen256Color,
    Xterm256Color,
    Conservative,
}

pub(crate) fn run() -> Result<u8> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    if let Some(command) = &cli.command {
        return run_command(command);
    }

    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("bmux warning: failed loading config, using defaults ({error})");
            BmuxConfig::default()
        }
    };

    let shell = resolve_shell(cli.shell);
    let runtime_settings = load_runtime_settings(&config);
    let runtime_options = RuntimeOptions {
        terminfo_auto_install: config.behavior.terminfo_auto_install,
        terminfo_prompt_cooldown_days: config.behavior.terminfo_prompt_cooldown_days.max(1),
    };

    let runtime_settings = maybe_install_terminfo_on_startup(runtime_settings, &runtime_options)?;
    debug!("Starting bmux runtime");
    debug!("Launching shell: {shell}");
    debug!(
        "Pane TERM configured='{}' effective='{}' profile='{}'",
        runtime_settings.configured_pane_term,
        runtime_settings.pane_term,
        terminal_profile_name(runtime_settings.terminal_profile)
    );
    for warning in &runtime_settings.warnings {
        eprintln!("bmux warning: {warning}");
    }

    run_two_pane_runtime(
        &shell,
        !cli.no_alt_screen,
        cli.debug_render,
        cli.debug_render_log.as_deref(),
        cli.debug_render_log_format,
        runtime_settings,
    )
}

fn run_command(command: &Command) -> Result<u8> {
    match command {
        Command::Server { command } => match command {
            ServerCommand::Start {
                daemon,
                foreground_internal,
            } => run_server_start(*daemon, *foreground_internal),
            ServerCommand::Status => run_server_status(),
            ServerCommand::Stop => run_server_stop(),
        },
        Command::Keymap { command } => match command {
            KeymapCommand::Doctor { json } => run_keymap_doctor(*json),
        },
        Command::Layout { command } => match command {
            LayoutCommand::Clear => run_layout_clear(),
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

fn run_server_start(daemon: bool, foreground_internal: bool) -> Result<u8> {
    if daemon && !foreground_internal {
        let executable = std::env::current_exe().context("failed to resolve bmux executable path")?;
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
        println!("bmux server started in daemon mode (pid {})", child.id());
        return Ok(0);
    }

    let server = BmuxServer::from_default_paths();
    write_server_pid_file(std::process::id())?;
    let run_result = run_async(async move { server.run().await });
    let _ = remove_server_pid_file();
    run_result?;
    Ok(0)
}

fn run_server_status() -> Result<u8> {
    let status = run_async(async {
        match tokio::time::timeout(
            SERVER_STATUS_TIMEOUT,
            BmuxClient::connect_default("bmux-cli-status"),
        )
        .await
        {
            Ok(Ok(mut client)) => client.server_status().await.map(Some).map_err(Into::into),
            Ok(Err(_)) | Err(_) => Ok(None),
        }
    })?;

    match status {
        Some(true) => {
            println!("bmux server is running");
            Ok(0)
        }
        _ => {
            println!("bmux server is not running");
            Ok(1)
        }
    }
}

fn run_server_stop() -> Result<u8> {
    let graceful_stopped = run_async(async {
        match tokio::time::timeout(SERVER_STOP_TIMEOUT, BmuxClient::connect_default("bmux-cli-stop")).await {
            Ok(Ok(mut client)) => {
                client.stop_server().await.map_err(anyhow::Error::from)?;
                for _ in 0..20 {
                    let reconnect = tokio::time::timeout(
                        SERVER_STATUS_TIMEOUT,
                        BmuxClient::connect_default("bmux-cli-stop-check"),
                    )
                    .await;
                    if reconnect.is_err() || matches!(reconnect, Ok(Err(_))) {
                        return Ok(true);
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Ok(false)
            }
            Ok(Err(_)) | Err(_) => Ok(false),
        }
    })?;

    if graceful_stopped {
        println!("bmux server stopped gracefully");
        let _ = remove_server_pid_file();
        return Ok(0);
    }

    if let Some(pid) = read_server_pid_file()? {
        if try_kill_pid(pid)? {
            println!("bmux server stop fallback succeeded (pid {pid})");
            let _ = remove_server_pid_file();
            return Ok(0);
        }
    }

    println!("bmux server is not running");
    Ok(1)
}

fn run_async<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;
    runtime.block_on(future)
}

fn server_pid_file_path() -> PathBuf {
    bmux_config::ConfigPaths::default().runtime_dir.join("server.pid")
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

    let pid = content
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid pid file content at {}", path.display()))?;
    Ok(Some(pid))
}

fn remove_server_pid_file() -> Result<()> {
    let path = server_pid_file_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed removing pid file {}", path.display()))
        }
    }
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

fn run_layout_clear() -> Result<u8> {
    let path = bmux_config::ConfigPaths::default().runtime_layout_state_file();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!("cleared persisted layout state at {}", path.display());
            Ok(0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("no persisted layout state found at {}", path.display());
            Ok(0)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed clearing persisted layout at {}", path.display())),
    }
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

fn run_two_pane_runtime(
    shell: &str,
    use_alt_screen: bool,
    debug_render: bool,
    debug_render_log: Option<&Path>,
    debug_render_log_format: DebugRenderLogFormat,
    runtime_settings: RuntimeSettings,
) -> Result<u8> {
    let terminal_guard = TerminalGuard::activate(use_alt_screen, true)?;

    let (mut cols, mut rows) =
        crossterm::terminal::size().context("failed to read terminal size")?;
    let startup_deadline = Instant::now() + STARTUP_ALT_SCREEN_GUARD_DURATION;
    let user_input_seen = Arc::new(AtomicBool::new(false));
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let protocol_trace = if runtime_settings.protocol_trace_enabled {
        Some(Arc::new(Mutex::new(ProtocolTraceBuffer::with_capacity(
            runtime_settings.protocol_trace_capacity,
        ))))
    } else {
        None
    };
    let (mut layout_tree, mut panes) = initialize_runtime_state(
        shell,
        runtime_settings.scrollback_limit,
        &runtime_settings.pane_term,
        runtime_settings.protocol_profile,
        cols,
        rows,
        startup_deadline,
        Arc::clone(&user_input_seen),
        runtime_settings.layout_persistence_enabled,
        protocol_trace.clone(),
    )?;
    let mut pane_rects = layout_tree.compute_rects(cols, rows);
    let mut last_persisted_at = Instant::now();

    let (input_tx, input_rx) = mpsc::channel::<RuntimeAction>();
    let input_thread = spawn_input_thread(
        input_tx,
        runtime_settings.keymap,
        Arc::clone(&user_input_seen),
        Arc::clone(&shutdown_requested),
    )?;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
    let shell_name = shell_name(shell);
    let mut focused_pane = layout_tree.focused;
    let mut force_redraw = true;
    let mut kill_sent = false;
    let mut next_status_redraw = Instant::now() + STATUS_REDRAW_INTERVAL;
    let mut exit_override = None;
    let mut status_message: Option<StatusMessage> = None;
    let mut render_cache = RenderCache::default();
    let mut render_debug =
        RenderDebugState::new(debug_render, debug_render_log, debug_render_log_format)?;
    let mut scroll_state = ScrollState::default();
    let mut internal_clipboard: Option<String> = None;
    let mut persistence_dirty = true;

    let exit_code = loop {
        let focused_before_input = focused_pane;
        if let Some(updated_tree) = process_input_events(
            &input_rx,
            &mut panes,
            &pane_rects,
            &layout_tree,
            &mut focused_pane,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            startup_deadline,
            Arc::clone(&user_input_seen),
            runtime_settings.scrollback_limit,
            &runtime_settings.pane_term,
            runtime_settings.protocol_profile,
            protocol_trace.clone(),
        )? {
            layout_tree = updated_tree;
            layout_tree.focused = focused_pane;
            pane_rects = layout_tree.compute_rects(cols, rows);
            resize_panes(&mut panes, &pane_rects)?;
            terminal_guard.refresh_layout(rows)?;
            force_redraw = true;
            persistence_dirty = true;
        }

        if focused_pane != focused_before_input {
            persistence_dirty = true;
        }

        if shutdown_requested.load(Ordering::Relaxed) && !kill_sent {
            debug!("Terminating pane shells");
            for pane in panes.values_mut() {
                stop_pane_process(pane, true)?;
            }
            kill_sent = true;
        }

        refresh_exit_codes(&mut panes)?;
        let reap_result = reap_exited_panes(&mut panes, &mut layout_tree, &mut focused_pane);
        if let Some(code) = reap_result.session_exit_code {
            break code;
        }
        scroll_state
            .offsets
            .retain(|pane_id, _| panes.contains_key(pane_id));
        if panes.is_empty() {
            scroll_state.active = false;
        }
        if reap_result.removed_any {
            pane_rects = layout_tree.compute_rects(cols, rows);
            resize_panes(&mut panes, &pane_rects)?;
            terminal_guard.refresh_layout(rows)?;
            force_redraw = true;
            persistence_dirty = true;
        }

        if !panes.get(&focused_pane).is_some_and(pane_is_running) {
            if let Some(next_focus) = first_running_pane_id(&layout_tree.pane_order(), &panes) {
                focused_pane = next_focus;
                layout_tree.focused = focused_pane;
                persistence_dirty = true;
            }
        }

        if shutdown_requested.load(Ordering::Relaxed) && !any_running_panes(&panes) {
            break exit_override.unwrap_or(0);
        }

        if status_message
            .as_ref()
            .is_some_and(status_message::is_expired)
        {
            status_message = None;
            force_redraw = true;
        }

        let (new_cols, new_rows) =
            crossterm::terminal::size().context("failed to read terminal size")?;
        if (new_cols, new_rows) != (cols, rows) {
            cols = new_cols;
            rows = new_rows;
            pane_rects = layout_tree.compute_rects(cols, rows);
            resize_panes(&mut panes, &pane_rects)?;
            terminal_guard.refresh_layout(rows)?;
            force_redraw = true;
            next_status_redraw = Instant::now() + STATUS_REDRAW_INTERVAL;
            debug!("Terminal resized to {cols}x{rows}");
        }

        let layout_for_ratio = layout_tree.compute_rects(cols, rows);
        if layout_for_ratio != pane_rects {
            pane_rects = layout_for_ratio;
            resize_panes(&mut panes, &pane_rects)?;
            terminal_guard.refresh_layout(rows)?;
            force_redraw = true;
        }

        let pane_dirty = panes
            .values()
            .any(|pane| pane.state.dirty.swap(false, Ordering::Relaxed));

        if runtime_settings.layout_persistence_enabled
            && persistence_dirty
            && last_persisted_at.elapsed() >= STATUS_REDRAW_INTERVAL
        {
            if let Err(error) = save_persisted_runtime_state(&layout_tree, &panes, focused_pane) {
                eprintln!("bmux warning: failed to persist runtime layout ({error})");
            } else {
                persistence_dirty = false;
                last_persisted_at = Instant::now();
            }
        }

        let scroll_status_suffix = if scroll_state.active {
            let (offset, total) = panes
                .get(&focused_pane)
                .map(|pane| {
                    let mut parser = pane
                        .state
                        .parser
                        .lock()
                        .expect("pane parser mutex poisoned");
                    let offset = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(usize::MAX);
                    let total = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(offset);
                    (offset, total)
                })
                .or_else(|| {
                    scroll_state
                        .offsets
                        .get(&focused_pane)
                        .copied()
                        .map(|offset| (offset, offset))
                })
                .unwrap_or((0, 0));

            Some(format_scroll_mode_suffix(
                offset,
                total,
                selection_status_suffix(&scroll_state, focused_pane),
            ))
        } else {
            None
        };

        let selection_overlay = if scroll_state.active {
            scroll_state
                .selection_anchors
                .get(&focused_pane)
                .zip(scroll_state.cursors.get(&focused_pane))
                .map(|(anchor, cursor)| SelectionOverlay {
                    pane_id: focused_pane,
                    start_row: anchor.0,
                    start_col: anchor.1,
                    end_row: cursor.0,
                    end_col: cursor.1,
                })
        } else {
            None
        };

        if force_redraw || pane_dirty || Instant::now() >= next_status_redraw {
            render_frame(
                &panes,
                &pane_rects,
                cols,
                rows,
                shell_name,
                &cwd,
                focused_pane,
                selection_overlay,
                scroll_status_suffix.as_deref(),
                status_message.as_ref().map(|message| message.text.as_str()),
                force_redraw,
                &mut render_cache,
                &mut render_debug,
            )?;
            force_redraw = false;
            next_status_redraw = Instant::now() + STATUS_REDRAW_INTERVAL;
        }

        thread::sleep(FRAME_INTERVAL);
    };

    if runtime_settings.layout_persistence_enabled
        && let Err(error) = save_persisted_runtime_state(&layout_tree, &panes, focused_pane)
    {
        eprintln!("bmux warning: failed to persist runtime layout on shutdown ({error})");
    }

    if let Some(trace) = &protocol_trace
        && let Err(error) = save_protocol_trace(trace)
    {
        eprintln!("bmux warning: failed persisting protocol trace ({error})");
    }

    shutdown_requested.store(true, Ordering::Relaxed);
    if input_thread.is_finished() {
        match input_thread.join() {
            Ok(result) => result.context("PTY input thread failed")?,
            Err(_) => return Err(anyhow::anyhow!("PTY input thread panicked")),
        }
    } else {
        debug!("Input thread still blocked on stdin; skipping join during shutdown");
    }

    for pane in panes.values_mut() {
        stop_pane_process(pane, false)?;
    }

    Ok(exit_override.unwrap_or(exit_code))
}

fn selection_status_suffix(scroll_state: &ScrollState, focused_pane: PaneId) -> Option<String> {
    let (anchor, cursor) = scroll_state
        .selection_anchors
        .get(&focused_pane)
        .zip(scroll_state.cursors.get(&focused_pane))?;

    let (start, end) = if anchor <= cursor {
        (*anchor, *cursor)
    } else {
        (*cursor, *anchor)
    };

    Some(format!(
        "SELECT r{}:c{} -> r{}:c{}",
        start.0.saturating_add(1),
        start.1.saturating_add(1),
        end.0.saturating_add(1),
        end.1.saturating_add(1)
    ))
}

fn format_scroll_mode_suffix(
    offset: usize,
    total: usize,
    selection_suffix: Option<String>,
) -> String {
    let mut status = format!("SCROLL {offset}/{total}");
    if let Some(selection) = selection_suffix {
        status.push_str(" | ");
        status.push_str(&selection);
    }
    status
}

fn initialize_runtime_state(
    shell: &str,
    scrollback_limit: usize,
    pane_term: &str,
    protocol_profile: ProtocolProfile,
    cols: u16,
    rows: u16,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
    persistence_enabled: bool,
    protocol_trace: Option<SharedProtocolTraceBuffer>,
) -> Result<(LayoutTree, BTreeMap<PaneId, PaneRuntime>)> {
    let restored = if persistence_enabled {
        match load_persisted_runtime_state() {
            Ok(state) => state,
            Err(error) => {
                eprintln!("bmux warning: failed loading persisted runtime layout ({error})");
                None
            }
        }
    } else {
        None
    };

    let layout_tree = restored.as_ref().map_or_else(
        || LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5),
        |state| state.layout_tree.clone(),
    );
    let pane_rects = layout_tree.compute_rects(cols, rows);
    let pane_order = layout_tree.pane_order();

    let mut panes = BTreeMap::new();
    for pane_id in pane_order {
        let (title, pane_shell) = if let Some(state) = restored.as_ref() {
            if let Some(meta) = state.panes.get(&pane_id) {
                (meta.title.clone(), meta.shell.clone())
            } else {
                (format!("pane-{}", pane_id.0), shell.to_string())
            }
        } else {
            match pane_id.0 {
                1 => ("left".to_string(), shell.to_string()),
                2 => ("right".to_string(), shell.to_string()),
                _ => (format!("pane-{}", pane_id.0), shell.to_string()),
            }
        };

        panes.insert(
            pane_id,
            spawn_pane(
                pane_id,
                &pane_shell,
                scrollback_limit,
                pane_term,
                protocol_profile,
                title,
                pane_rects[&pane_id].inner(),
                startup_deadline,
                Arc::clone(&user_input_seen),
                protocol_trace.clone(),
            )?,
        );
    }

    Ok((layout_tree, panes))
}

fn spawn_input_thread(
    input_tx: Sender<RuntimeAction>,
    keymap: crate::input::Keymap,
    user_input_seen: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<Result<()>>> {
    let input_thread = thread::Builder::new()
        .name("bmux-pty-input".to_string())
        .spawn(move || -> Result<()> {
            let mut processor = InputProcessor::new(keymap);

            if io::stdin().is_terminal() {
                run_event_input_loop(
                    &input_tx,
                    &shutdown_requested,
                    &user_input_seen,
                    &mut processor,
                )?;
            } else {
                run_stream_input_loop(
                    &input_tx,
                    &shutdown_requested,
                    &user_input_seen,
                    &mut processor,
                )?;
            }

            if let Some(trailing_action) = processor.finish() {
                let _ = input_tx.send(trailing_action);
            }

            Ok(())
        })
        .context("failed to spawn PTY input thread")?;

    Ok(input_thread)
}

fn run_event_input_loop(
    input_tx: &Sender<RuntimeAction>,
    shutdown_requested: &Arc<AtomicBool>,
    user_input_seen: &Arc<AtomicBool>,
    processor: &mut InputProcessor,
) -> Result<()> {
    let mut reader = CrosstermEventReader;
    run_event_input_loop_with_reader(
        &mut reader,
        input_tx,
        shutdown_requested,
        user_input_seen,
        processor,
    )
}

trait EventReader {
    fn poll(&mut self, timeout: Duration) -> Result<bool>;
    fn read(&mut self) -> Result<Event>;
}

struct CrosstermEventReader;

impl EventReader for CrosstermEventReader {
    fn poll(&mut self, timeout: Duration) -> Result<bool> {
        event::poll(timeout).context("failed polling terminal input")
    }

    fn read(&mut self) -> Result<Event> {
        event::read().context("failed reading terminal event")
    }
}

fn run_event_input_loop_with_reader<R: EventReader>(
    reader: &mut R,
    input_tx: &Sender<RuntimeAction>,
    shutdown_requested: &Arc<AtomicBool>,
    user_input_seen: &Arc<AtomicBool>,
    processor: &mut InputProcessor,
) -> Result<()> {
    loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            break;
        }

        if !reader.poll(INPUT_POLL_INTERVAL)? {
            continue;
        }

        let actions = processor.process_terminal_event(reader.read()?);
        if !actions.is_empty() {
            user_input_seen.store(true, Ordering::Relaxed);
            for action in actions {
                let _ = input_tx.send(action);
            }
        }
    }

    Ok(())
}

fn run_stream_input_loop(
    input_tx: &Sender<RuntimeAction>,
    shutdown_requested: &Arc<AtomicBool>,
    user_input_seen: &Arc<AtomicBool>,
    processor: &mut InputProcessor,
) -> Result<()> {
    let mut stdin = io::stdin().lock();
    let mut buffer = [0_u8; 8192];

    loop {
        if shutdown_requested.load(Ordering::Relaxed) {
            break;
        }

        let bytes_read = stdin
            .read(&mut buffer)
            .context("failed reading terminal input")?;

        if bytes_read == 0 {
            break;
        }

        user_input_seen.store(true, Ordering::Relaxed);
        for action in processor.process_stream_bytes(&buffer[..bytes_read]) {
            let _ = input_tx.send(action);
        }
    }

    Ok(())
}

fn load_runtime_settings(config: &BmuxConfig) -> RuntimeSettings {
    let (runtime_bindings, global_bindings) = merged_runtime_keybindings(config);
    let keymap = match crate::input::Keymap::from_parts(
        &config.keybindings.prefix,
        config.keybindings.timeout_ms,
        &runtime_bindings,
        &global_bindings,
    ) {
        Ok(keymap) => keymap,
        Err(error) => {
            eprintln!("bmux warning: invalid keymap config, using defaults ({error})");
            crate::input::Keymap::default_runtime()
        }
    };

    let configured_pane_term = config.behavior.pane_term.clone();
    let pane_term_resolution = resolve_pane_term(&configured_pane_term);
    let protocol_profile = protocol_profile_for_terminal_profile(pane_term_resolution.profile);

    RuntimeSettings {
        keymap,
        layout_persistence_enabled: config.behavior.restore_last_layout,
        scrollback_limit: config.general.scrollback_limit,
        pane_term: pane_term_resolution.pane_term,
        terminal_profile: pane_term_resolution.profile,
        protocol_profile,
        protocol_trace_enabled: config.behavior.protocol_trace_enabled,
        protocol_trace_capacity: config.behavior.protocol_trace_capacity.clamp(16, 10_000),
        configured_pane_term,
        warnings: pane_term_resolution.warnings,
    }
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

fn save_protocol_trace(trace: &SharedProtocolTraceBuffer) -> Result<()> {
    let path = bmux_config::ConfigPaths::default().protocol_trace_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed creating protocol trace dir at {}", parent.display())
        })?;
    }
    let (snapshot, dropped) = {
        let guard = trace.lock().expect("protocol trace mutex poisoned");
        (guard.snapshot(10_000), guard.dropped())
    };
    let payload = ProtocolTraceFile {
        dropped,
        events: snapshot,
    };
    let bytes = serde_json::to_vec_pretty(&payload).context("failed encoding protocol trace")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).with_context(|| {
        format!(
            "failed writing protocol trace tmp file at {}",
            tmp.display()
        )
    })?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed replacing protocol trace file at {}", path.display()))?;
    Ok(())
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

fn maybe_install_terminfo_on_startup(
    mut runtime_settings: RuntimeSettings,
    runtime_options: &RuntimeOptions,
) -> Result<RuntimeSettings> {
    if runtime_settings.configured_pane_term != "bmux-256color" {
        return Ok(runtime_settings);
    }
    if runtime_settings.pane_term == "bmux-256color" {
        return Ok(runtime_settings);
    }

    match runtime_options.terminfo_auto_install {
        TerminfoAutoInstall::Never => return Ok(runtime_settings),
        TerminfoAutoInstall::Always => {
            if let Err(error) = install_bmux_terminfo() {
                eprintln!("bmux warning: failed auto-installing terminfo ({error})");
                return Ok(runtime_settings);
            }
        }
        TerminfoAutoInstall::Ask => {
            if !io::stdin().is_terminal() {
                return Ok(runtime_settings);
            }

            if !prompt_allowed_by_cooldown(runtime_options.terminfo_prompt_cooldown_days)? {
                return Ok(runtime_settings);
            }

            println!(
                "bmux terminfo 'bmux-256color' is missing; install now for better compatibility? [Y/n]"
            );
            let mut answer = String::new();
            io::stdin()
                .read_line(&mut answer)
                .context("failed reading terminfo install prompt")?;
            let trimmed = answer.trim().to_ascii_lowercase();
            if trimmed == "n" || trimmed == "no" {
                persist_prompt_decline_now()?;
                return Ok(runtime_settings);
            }

            if let Err(error) = install_bmux_terminfo() {
                eprintln!("bmux warning: failed installing terminfo ({error})");
                return Ok(runtime_settings);
            }
        }
    }

    if check_terminfo_available("bmux-256color") == Some(true) {
        let resolved = resolve_pane_term("bmux-256color");
        runtime_settings.pane_term = resolved.pane_term;
        runtime_settings.terminal_profile = resolved.profile;
        runtime_settings.protocol_profile =
            protocol_profile_for_terminal_profile(runtime_settings.terminal_profile);
        runtime_settings.warnings = resolved.warnings;
    }

    Ok(runtime_settings)
}

fn prompt_allowed_by_cooldown(cooldown_days: u64) -> Result<bool> {
    let path = bmux_config::ConfigPaths::default().terminfo_prompt_state_file();
    if !path.exists() {
        return Ok(true);
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading terminfo prompt state at {}", path.display()))?;
    let state: TerminfoPromptStateFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed parsing terminfo prompt state at {}", path.display()))?;
    let Some(last) = state.last_declined_epoch_secs else {
        return Ok(true);
    };
    let now = unix_now_secs();
    let cooldown = cooldown_days.saturating_mul(24 * 60 * 60);
    Ok(now.saturating_sub(last) >= cooldown)
}

fn persist_prompt_decline_now() -> Result<()> {
    let path = bmux_config::ConfigPaths::default().terminfo_prompt_state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating prompt state dir at {}", parent.display()))?;
    }
    let state = TerminfoPromptStateFile {
        last_declined_epoch_secs: Some(unix_now_secs()),
    };
    let payload = serde_json::to_vec_pretty(&state).context("failed encoding prompt state")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload)
        .with_context(|| format!("failed writing prompt state tmp at {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed replacing prompt state at {}", path.display()))?;
    Ok(())
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

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
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

fn resolve_shell(cli_shell: Option<String>) -> String {
    if let Some(shell) = cli_shell {
        return shell;
    }

    if let Some(shell) = std::env::var_os("SHELL") {
        return shell.to_string_lossy().into_owned();
    }

    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn shell_name(shell: &str) -> &str {
    Path::new(shell)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell)
}

fn exit_code_from_u32(code: u32) -> u8 {
    match u8::try_from(code) {
        Ok(valid_code) => valid_code,
        Err(_) => u8::MAX,
    }
}

fn reap_exited_panes(
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    layout_tree: &mut LayoutTree,
    focused_pane: &mut PaneId,
) -> ReapExitedPanesResult {
    let exited: Vec<(PaneId, u8)> = panes
        .iter()
        .filter_map(|(pane_id, pane)| {
            if pane.process.is_none() {
                pane.exit_code.map(|code| (*pane_id, code))
            } else {
                None
            }
        })
        .collect();

    if exited.is_empty() {
        return ReapExitedPanesResult {
            removed_any: false,
            session_exit_code: None,
        };
    }

    let mut last_exit_code = None;
    for (pane_id, exit_code) in exited {
        let _ = panes.remove(&pane_id);
        let _ = layout_tree.remove_pane(pane_id);
        last_exit_code = Some(exit_code);
    }

    if panes.is_empty() {
        return ReapExitedPanesResult {
            removed_any: true,
            session_exit_code: Some(last_exit_code.unwrap_or(0)),
        };
    }

    if !panes.contains_key(focused_pane) {
        if let Some(next_focus) = layout_tree.pane_order().first().copied() {
            *focused_pane = next_focus;
            layout_tree.focused = next_focus;
        }
    }

    ReapExitedPanesResult {
        removed_any: true,
        session_exit_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EventReader, PaneRuntime, PaneState, ProtocolDirection, ProtocolTraceEvent, ScrollState,
        TerminalProfile, TraceFamily, filter_trace_events, format_scroll_mode_suffix,
        load_runtime_settings, merged_runtime_keybindings, profile_for_term,
        protocol_profile_for_terminal_profile, reap_exited_panes, resolve_pane_term_with_checker,
        run_event_input_loop_with_reader, selection_status_suffix,
    };
    use crate::input::{InputProcessor, Keymap};
    use crate::pane::{LayoutNode, LayoutTree, PaneId, SplitDirection};
    use bmux_config::BmuxConfig;
    use crossterm::event::Event;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use vt100::Parser as VtParser;

    struct MockEventReader {
        poll_calls: usize,
        shutdown_requested: Arc<AtomicBool>,
    }

    impl EventReader for MockEventReader {
        fn poll(&mut self, _timeout: Duration) -> anyhow::Result<bool> {
            self.poll_calls += 1;
            self.shutdown_requested.store(true, Ordering::Relaxed);
            Ok(false)
        }

        fn read(&mut self) -> anyhow::Result<Event> {
            panic!("read should not be called when poll returns false");
        }
    }

    fn make_inactive_pane(exit_code: Option<u8>) -> PaneRuntime {
        PaneRuntime {
            title: "pane".to_string(),
            shell: "/bin/sh".to_string(),
            state: Arc::new(PaneState {
                parser: Mutex::new(VtParser::new(10, 10, 100)),
                dirty: AtomicBool::new(false),
            }),
            process: None,
            closed: false,
            exit_code,
        }
    }

    #[test]
    fn reaps_exited_pane_and_moves_focus() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_inactive_pane(Some(7)));
        panes.insert(PaneId(2), make_inactive_pane(None));

        let mut tree = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let mut focused = PaneId(1);

        let result = reap_exited_panes(&mut panes, &mut tree, &mut focused);
        assert!(result.removed_any);
        assert_eq!(result.session_exit_code, None);
        assert!(!panes.contains_key(&PaneId(1)));
        assert_eq!(tree.pane_order(), vec![PaneId(2)]);
        assert_eq!(focused, PaneId(2));
    }

    #[test]
    fn returns_last_exit_code_when_final_pane_exits() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_inactive_pane(Some(42)));

        let mut tree = LayoutTree {
            root: LayoutNode::Leaf { pane_id: PaneId(1) },
            focused: PaneId(1),
        };
        let mut focused = PaneId(1);

        let result = reap_exited_panes(&mut panes, &mut tree, &mut focused);
        assert!(result.removed_any);
        assert_eq!(result.session_exit_code, Some(42));
        assert!(panes.is_empty());
    }

    #[test]
    fn event_loop_observes_shutdown_without_blocking() {
        let (tx, rx) = mpsc::channel();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let user_input_seen = Arc::new(AtomicBool::new(false));
        let mut processor = InputProcessor::new(Keymap::default_runtime());
        let mut reader = MockEventReader {
            poll_calls: 0,
            shutdown_requested: Arc::clone(&shutdown_requested),
        };

        run_event_input_loop_with_reader(
            &mut reader,
            &tx,
            &shutdown_requested,
            &user_input_seen,
            &mut processor,
        )
        .expect("event loop should exit cleanly");

        assert_eq!(reader.poll_calls, 1);
        assert!(shutdown_requested.load(Ordering::Relaxed));
        assert!(!user_input_seen.load(Ordering::Relaxed));
        assert!(rx.try_recv().is_err());
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
    fn runtime_settings_uses_configured_scrollback_limit() {
        let mut config = BmuxConfig::default();
        config.general.scrollback_limit = 4_321;

        let settings = load_runtime_settings(&config);
        assert_eq!(settings.scrollback_limit, 4_321);
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
    fn scroll_mode_suffix_includes_position_and_total() {
        let suffix = format_scroll_mode_suffix(0, 3_000, None);
        assert_eq!(suffix, "SCROLL 0/3000");
    }

    #[test]
    fn selection_suffix_uses_ordered_one_based_coordinates() {
        let mut scroll_state = ScrollState::default();
        let pane_id = PaneId(7);
        scroll_state.selection_anchors.insert(pane_id, (10, 20));
        scroll_state.cursors.insert(pane_id, (2, 4));

        let suffix = selection_status_suffix(&scroll_state, pane_id);
        assert_eq!(suffix.as_deref(), Some("SELECT r3:c5 -> r11:c21"));
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
}
