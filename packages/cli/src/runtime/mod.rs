use crate::cli::{Cli, Command, DebugRenderLogFormat, KeymapCommand};
use crate::input::{InputProcessor, RuntimeAction};
use crate::pane::compute_vertical_layout;
use crate::pty::STARTUP_ALT_SCREEN_GUARD_DURATION;
use crate::terminal::TerminalGuard;
use anyhow::{Context, Result};
use bmux_config::BmuxConfig;
use clap::Parser;
use portable_pty::{Child, MasterPty};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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
mod status_message;
use commands::process_input_events;
use compositor::{RenderCache, RenderDebugState, render_frame};
use pane_runtime::{
    any_running_panes, first_running_pane_index, pane_is_running, refresh_exit_codes, resize_panes,
    spawn_pane, stop_pane_process,
};
use status_message::StatusMessage;

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const STATUS_REDRAW_INTERVAL: Duration = Duration::from_millis(200);
const SPLIT_RATIO_STEP: f32 = 0.05;
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

pub(crate) fn run() -> Result<u8> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    if let Some(command) = &cli.command {
        return run_command(command);
    }

    let shell = resolve_shell(cli.shell);
    let keymap = load_runtime_keymap();
    debug!("Starting bmux runtime");
    debug!("Launching shell: {shell}");

    run_two_pane_runtime(
        &shell,
        !cli.no_alt_screen,
        cli.debug_render,
        cli.debug_render_log.as_deref(),
        cli.debug_render_log_format,
        keymap,
    )
}

fn run_command(command: &Command) -> Result<u8> {
    match command {
        Command::Keymap { command } => match command {
            KeymapCommand::Doctor { json } => run_keymap_doctor(*json),
        },
    }
}

fn run_two_pane_runtime(
    shell: &str,
    use_alt_screen: bool,
    debug_render: bool,
    debug_render_log: Option<&Path>,
    debug_render_log_format: DebugRenderLogFormat,
    keymap: crate::input::Keymap,
) -> Result<u8> {
    let terminal_guard = TerminalGuard::activate(use_alt_screen, true)?;

    let (mut cols, mut rows) =
        crossterm::terminal::size().context("failed to read terminal size")?;
    let mut split_ratio = 0.5_f32;
    let mut layout = compute_vertical_layout(cols, rows, split_ratio);

    let startup_deadline = Instant::now() + STARTUP_ALT_SCREEN_GUARD_DURATION;
    let user_input_seen = Arc::new(AtomicBool::new(false));
    let shutdown_requested = Arc::new(AtomicBool::new(false));

    let mut panes = vec![
        spawn_pane(
            shell,
            "left".to_string(),
            layout.left.inner(),
            startup_deadline,
            Arc::clone(&user_input_seen),
        )?,
        spawn_pane(
            shell,
            "right".to_string(),
            layout.right.inner(),
            startup_deadline,
            Arc::clone(&user_input_seen),
        )?,
    ];

    let (input_tx, input_rx) = mpsc::channel::<RuntimeAction>();
    let input_thread = spawn_input_thread(
        input_tx,
        keymap,
        Arc::clone(&user_input_seen),
        Arc::clone(&shutdown_requested),
    )?;

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
    let shell_name = shell_name(shell);
    let mut focused_pane = 0_usize;
    let mut force_redraw = true;
    let mut kill_sent = false;
    let mut next_status_redraw = Instant::now() + STATUS_REDRAW_INTERVAL;
    let mut exit_override = None;
    let mut status_message: Option<StatusMessage> = None;
    let mut render_cache = RenderCache::default();
    let mut render_debug =
        RenderDebugState::new(debug_render, debug_render_log, debug_render_log_format)?;

    let exit_code = loop {
        process_input_events(
            &input_rx,
            &mut panes,
            &layout,
            &mut focused_pane,
            &mut split_ratio,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            startup_deadline,
            Arc::clone(&user_input_seen),
        )?;

        if shutdown_requested.load(Ordering::Relaxed) && !kill_sent {
            debug!("Terminating pane shells");
            for pane in &mut panes {
                stop_pane_process(pane, true)?;
            }
            kill_sent = true;
        }

        refresh_exit_codes(&mut panes)?;
        if focused_pane >= panes.len() || !pane_is_running(&panes[focused_pane]) {
            if let Some(next_focus) = first_running_pane_index(&panes) {
                focused_pane = next_focus;
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
            layout = compute_vertical_layout(cols, rows, split_ratio);
            resize_panes(&mut panes, &layout)?;
            terminal_guard.refresh_layout(rows)?;
            force_redraw = true;
            next_status_redraw = Instant::now() + STATUS_REDRAW_INTERVAL;
            debug!("Terminal resized to {cols}x{rows}");
        }

        let layout_for_ratio = compute_vertical_layout(cols, rows, split_ratio);
        if layout_for_ratio != layout {
            layout = layout_for_ratio;
            resize_panes(&mut panes, &layout)?;
            terminal_guard.refresh_layout(rows)?;
            force_redraw = true;
        }

        let pane_dirty = panes
            .iter()
            .any(|pane| pane.state.dirty.swap(false, Ordering::Relaxed));

        if force_redraw || pane_dirty || Instant::now() >= next_status_redraw {
            render_frame(
                &panes,
                &layout,
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

    match input_thread.join() {
        Ok(result) => result.context("PTY input thread failed")?,
        Err(_) => return Err(anyhow::anyhow!("PTY input thread panicked")),
    }

    for pane in &mut panes {
        stop_pane_process(pane, false)?;
    }

    Ok(exit_override.unwrap_or(exit_code))
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
            let mut stdin = io::stdin().lock();
            let mut buffer = [0_u8; 8192];
            let mut processor = InputProcessor::new(keymap);

            loop {
                if shutdown_requested.load(Ordering::Relaxed) {
                    break;
                }

                let bytes_read = stdin
                    .read(&mut buffer)
                    .context("failed reading terminal input")?;

                if bytes_read == 0 {
                    if let Some(trailing_action) = processor.finish() {
                        let _ = input_tx.send(trailing_action);
                    }
                    let _ = input_tx.send(RuntimeAction::Eof);
                    break;
                }

                user_input_seen.store(true, Ordering::Relaxed);

                for action in processor.process_chunk(&buffer[..bytes_read]) {
                    let _ = input_tx.send(action);
                }
            }

            Ok(())
        })
        .context("failed to spawn PTY input thread")?;

    Ok(input_thread)
}

fn load_runtime_keymap() -> crate::input::Keymap {
    match BmuxConfig::load() {
        Ok(config) => match crate::input::Keymap::from_parts(
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
        },
        Err(error) => {
            eprintln!("bmux warning: failed loading config, using default keymap ({error})");
            crate::input::Keymap::default_runtime()
        }
    }
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
