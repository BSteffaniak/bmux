use crate::cli::{
    Cli, Command, DebugRenderLogFormat, KeymapCommand, LayoutCommand, TerminalCommand,
};
use crate::input::{InputProcessor, RuntimeAction};
use crate::pane::{LayoutTree, PaneId, SplitDirection};
use crate::pty::STARTUP_ALT_SCREEN_GUARD_DURATION;
use crate::terminal::TerminalGuard;
use anyhow::{Context, Result};
use bmux_config::BmuxConfig;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{Child, MasterPty};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
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
use commands::process_input_events;
use compositor::{RenderCache, RenderDebugState, render_frame};
use pane_runtime::{
    any_running_panes, first_running_pane_id, pane_is_running, refresh_exit_codes, resize_panes,
    spawn_pane, stop_pane_process,
};
use persistence::{load_persisted_runtime_state, save_persisted_runtime_state};
use status_message::StatusMessage;

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STATUS_REDRAW_INTERVAL: Duration = Duration::from_millis(200);
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
    writer: Box<dyn Write + Send>,
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
    pane_term: String,
    terminal_profile: TerminalProfile,
    configured_pane_term: String,
    warnings: Vec<String>,
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

    let shell = resolve_shell(cli.shell);
    let runtime_settings = load_runtime_settings();
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
        Command::Keymap { command } => match command {
            KeymapCommand::Doctor { json } => run_keymap_doctor(*json),
        },
        Command::Layout { command } => match command {
            LayoutCommand::Clear => run_layout_clear(),
        },
        Command::Terminal { command } => match command {
            TerminalCommand::Doctor { json } => run_terminal_doctor(*json),
        },
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

fn run_terminal_doctor(as_json: bool) -> Result<u8> {
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

    if as_json {
        let payload = serde_json::json!({
            "configured_pane_term": configured_term,
            "effective_pane_term": effective.pane_term,
            "terminal_profile": terminal_profile_name(effective.profile),
            "terminfo_check": {
                "attempted": effective.terminfo_checked,
                "available": effective.terminfo_available,
            },
            "warnings": effective.warnings,
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
    if effective.terminfo_checked {
        println!(
            "terminfo available: {}",
            if effective.terminfo_available {
                "yes"
            } else {
                "no"
            }
        );
    }
    for warning in effective.warnings {
        println!("warning: {warning}");
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
    let (mut layout_tree, mut panes) = initialize_runtime_state(
        shell,
        &runtime_settings.pane_term,
        cols,
        rows,
        startup_deadline,
        Arc::clone(&user_input_seen),
        runtime_settings.layout_persistence_enabled,
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
            startup_deadline,
            Arc::clone(&user_input_seen),
            &runtime_settings.pane_term,
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

        if force_redraw || pane_dirty || Instant::now() >= next_status_redraw {
            render_frame(
                &panes,
                &pane_rects,
                cols,
                rows,
                shell_name,
                &cwd,
                focused_pane,
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

fn initialize_runtime_state(
    shell: &str,
    pane_term: &str,
    cols: u16,
    rows: u16,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
    persistence_enabled: bool,
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
                &pane_shell,
                pane_term,
                title,
                pane_rects[&pane_id].inner(),
                startup_deadline,
                Arc::clone(&user_input_seen),
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

        if let Some(bytes) = event_to_bytes(reader.read()?) {
            user_input_seen.store(true, Ordering::Relaxed);
            for action in processor.process_chunk(&bytes) {
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
        for action in processor.process_chunk(&buffer[..bytes_read]) {
            let _ = input_tx.send(action);
        }
    }

    Ok(())
}

fn event_to_bytes(event: Event) -> Option<Vec<u8>> {
    match event {
        Event::Key(key) => key_to_bytes(key),
        _ => None,
    }
}

fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let modifiers = key.modifiers;
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let shift = modifiers.contains(KeyModifiers::SHIFT);

    let mut out = Vec::new();

    let mut push_alt = || {
        if alt {
            out.push(0x1b);
        }
    };

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    push_alt();
                    out.push((lower as u8 - b'a') + 1);
                    return Some(out);
                }
            }

            push_alt();
            if c.is_ascii() {
                out.push(c as u8);
            } else {
                let mut buf = [0_u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(out)
        }
        KeyCode::Enter => {
            push_alt();
            out.push(b'\r');
            Some(out)
        }
        KeyCode::Tab => {
            push_alt();
            out.push(b'\t');
            Some(out)
        }
        KeyCode::Backspace => {
            push_alt();
            out.push(0x7f);
            Some(out)
        }
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'A']
        } else {
            vec![0x1b, b'[', b'A']
        }),
        KeyCode::Down => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'B']
        } else {
            vec![0x1b, b'[', b'B']
        }),
        KeyCode::Right => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'C']
        } else {
            vec![0x1b, b'[', b'C']
        }),
        KeyCode::Left => Some(if shift {
            vec![0x1b, b'[', b'1', b';', b'2', b'D']
        } else {
            vec![0x1b, b'[', b'D']
        }),
        KeyCode::Home => Some(vec![0x1b, b'[', b'H']),
        KeyCode::End => Some(vec![0x1b, b'[', b'F']),
        KeyCode::PageUp => Some(vec![0x1b, b'[', b'5', b'~']),
        KeyCode::PageDown => Some(vec![0x1b, b'[', b'6', b'~']),
        KeyCode::Insert => Some(vec![0x1b, b'[', b'2', b'~']),
        KeyCode::Delete => Some(vec![0x1b, b'[', b'3', b'~']),
        KeyCode::F(n) => match n {
            1 => Some(vec![0x1b, b'O', b'P']),
            2 => Some(vec![0x1b, b'O', b'Q']),
            3 => Some(vec![0x1b, b'O', b'R']),
            4 => Some(vec![0x1b, b'O', b'S']),
            _ => None,
        },
        _ => None,
    }
}

fn load_runtime_settings() -> RuntimeSettings {
    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("bmux warning: failed loading config, using defaults ({error})");
            BmuxConfig::default()
        }
    };

    let keymap = match crate::input::Keymap::from_parts(
        &config.keybindings.prefix,
        config.keybindings.timeout_ms,
        &config.keybindings.runtime,
        &config.keybindings.global,
    ) {
        Ok(keymap) => keymap,
        Err(error) => {
            eprintln!("bmux warning: invalid keymap config, using defaults ({error})");
            crate::input::Keymap::default_runtime()
        }
    };

    let configured_pane_term = config.behavior.pane_term.clone();
    let pane_term_resolution = resolve_pane_term(&configured_pane_term);

    RuntimeSettings {
        keymap,
        layout_persistence_enabled: config.behavior.restore_last_layout,
        pane_term: pane_term_resolution.pane_term,
        terminal_profile: pane_term_resolution.profile,
        configured_pane_term,
        warnings: pane_term_resolution.warnings,
    }
}

struct PaneTermResolution {
    pane_term: String,
    profile: TerminalProfile,
    warnings: Vec<String>,
    terminfo_checked: bool,
    terminfo_available: bool,
}

fn resolve_pane_term(configured: &str) -> PaneTermResolution {
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

    let mut pane_term = configured_normalized.clone();
    let mut profile = profile_for_term(&pane_term);

    let terminfo_check = check_terminfo_available(&pane_term);
    if pane_term == "bmux-256color" && terminfo_check == Some(false) {
        warnings.push(
            "terminfo for bmux-256color not found; falling back to xterm-256color".to_string(),
        );
        pane_term = "xterm-256color".to_string();
        profile = profile_for_term(&pane_term);
    }

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
        terminfo_checked: terminfo_check.is_some(),
        terminfo_available: terminfo_check.unwrap_or(false),
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
    let keymap = crate::input::Keymap::from_parts(
        &config.keybindings.prefix,
        config.keybindings.timeout_ms,
        &config.keybindings.runtime,
        &config.keybindings.global,
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
        EventReader, PaneRuntime, PaneState, TerminalProfile, key_to_bytes, profile_for_term,
        reap_exited_panes,
        run_event_input_loop_with_reader,
    };
    use crate::input::{InputProcessor, Keymap};
    use crate::pane::{LayoutNode, LayoutTree, PaneId, SplitDirection};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
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
    fn key_to_bytes_encodes_ctrl_characters() {
        let key = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert_eq!(key_to_bytes(key), Some(vec![0x03]));
    }

    #[test]
    fn key_to_bytes_encodes_arrow_sequences() {
        let key = KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert_eq!(key_to_bytes(key), Some(vec![0x1b, b'[', b'A']));
    }

    #[test]
    fn key_to_bytes_encodes_shift_arrow_sequences() {
        let key = KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert_eq!(
            key_to_bytes(key),
            Some(vec![0x1b, b'[', b'1', b';', b'2', b'D'])
        );
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
        assert_eq!(profile_for_term("bmux-256color"), TerminalProfile::Bmux256Color);
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
        assert_eq!(profile_for_term("weird-term"), TerminalProfile::Conservative);
    }
}
