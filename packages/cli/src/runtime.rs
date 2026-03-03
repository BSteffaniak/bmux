use crate::cli::{Cli, Command, DebugRenderLogFormat, KeymapCommand};
use crate::input::{InputProcessor, RuntimeAction};
use crate::pane::{compute_vertical_layout, Layout, Rect};
use crate::pty::{extract_filtered_output, STARTUP_ALT_SCREEN_GUARD_DURATION};
use crate::status::{build_status_line, write_status_line};
use crate::terminal::TerminalGuard;
use anyhow::{Context, Result};
use bmux_config::BmuxConfig;
use clap::Parser;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::debug;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vt100::Parser as VtParser;
use vt100::{Color, Screen};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const STATUS_REDRAW_INTERVAL: Duration = Duration::from_millis(200);
const SPLIT_RATIO_STEP: f32 = 0.05;
const MIN_PANE_ROWS: u16 = 2;
const MIN_PANE_COLS: u16 = 2;
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(3);

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

struct StatusMessage {
    text: String,
    expires_at: Instant,
}

#[derive(Default)]
struct RenderCache {
    initialized: bool,
    status_line: String,
    pane_rects: [Rect; 2],
    pane_titles: [String; 2],
    focused_pane: usize,
    pane_lines: [Vec<Vec<u8>>; 2],
}

struct PaneRenderData {
    rect: Rect,
    title: String,
    lines: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CellStyle {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

struct RenderDebugState {
    enabled: bool,
    window_start: Instant,
    frames: u32,
    changed_lines: usize,
    status_updates: u32,
    border_updates: u32,
    snapshot: String,
    logger: Option<RenderDebugLogger>,
}

struct RenderDebugLogger {
    file: std::fs::File,
    started_at: Instant,
    format: DebugRenderLogFormat,
}

impl RenderDebugState {
    fn new(
        enabled: bool,
        log_path: Option<&Path>,
        log_format: DebugRenderLogFormat,
    ) -> Result<Self> {
        let logger = if let Some(path) = log_path {
            Some(RenderDebugLogger::new(path, log_format)?)
        } else {
            None
        };

        Ok(Self {
            enabled,
            window_start: Instant::now(),
            frames: 0,
            changed_lines: 0,
            status_updates: 0,
            border_updates: 0,
            snapshot: String::new(),
            logger,
        })
    }

    fn record_frame(&mut self, changed_lines: usize, status_updated: bool, border_updated: bool) {
        if let Some(logger) = self.logger.as_mut() {
            let elapsed_ms = logger.started_at.elapsed().as_millis();
            let _ = logger.write_frame(elapsed_ms, changed_lines, status_updated, border_updated);
        }

        if !self.enabled {
            return;
        }

        self.frames = self.frames.saturating_add(1);
        self.changed_lines = self.changed_lines.saturating_add(changed_lines);
        if status_updated {
            self.status_updates = self.status_updates.saturating_add(1);
        }
        if border_updated {
            self.border_updates = self.border_updates.saturating_add(1);
        }

        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let fps = f64::from(self.frames) / elapsed.as_secs_f64();
            let lines_per_frame = if self.frames == 0 {
                0.0
            } else {
                self.changed_lines as f64 / f64::from(self.frames)
            };

            self.snapshot = format!(
                "render: {fps:.1}fps | lines/frame: {lines_per_frame:.1} | status: {} | borders: {}",
                self.status_updates, self.border_updates
            );

            if let Some(logger) = self.logger.as_mut() {
                let elapsed_ms = logger.started_at.elapsed().as_millis();
                let _ = logger.write_window(
                    elapsed_ms,
                    fps,
                    lines_per_frame,
                    self.status_updates,
                    self.border_updates,
                );
            }

            self.frames = 0;
            self.changed_lines = 0;
            self.status_updates = 0;
            self.border_updates = 0;
            self.window_start = Instant::now();
        }
    }

    fn snapshot(&self) -> Option<&str> {
        if self.enabled {
            Some(&self.snapshot)
        } else {
            None
        }
    }
}

impl RenderDebugLogger {
    fn new(path: &Path, format: DebugRenderLogFormat) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed opening render debug log at {}", path.display()))?;

        match format {
            DebugRenderLogFormat::Text => {
                let _ = writeln!(file, "# bmux render debug log");
            }
            DebugRenderLogFormat::Csv => {
                let _ = writeln!(
                    file,
                    "event,t_ms,changed_lines,status_updated,border_updated,fps,lines_per_frame,status_updates,border_updates"
                );
            }
        }

        Ok(Self {
            file,
            started_at: Instant::now(),
            format,
        })
    }

    fn write_frame(
        &mut self,
        elapsed_ms: u128,
        changed_lines: usize,
        status_updated: bool,
        border_updated: bool,
    ) -> std::io::Result<()> {
        match self.format {
            DebugRenderLogFormat::Text => writeln!(
                self.file,
                "t={}ms frame changed_lines={} status_updated={} border_updated={}",
                elapsed_ms, changed_lines, status_updated, border_updated
            ),
            DebugRenderLogFormat::Csv => writeln!(
                self.file,
                "frame,{},{},{},{},,,,",
                elapsed_ms, changed_lines, status_updated, border_updated
            ),
        }
    }

    fn write_window(
        &mut self,
        elapsed_ms: u128,
        fps: f64,
        lines_per_frame: f64,
        status_updates: u32,
        border_updates: u32,
    ) -> std::io::Result<()> {
        match self.format {
            DebugRenderLogFormat::Text => writeln!(
                self.file,
                "t={}ms window fps={:.3} lines_per_frame={:.3} status_updates={} border_updates={}",
                elapsed_ms, fps, lines_per_frame, status_updates, border_updates
            ),
            DebugRenderLogFormat::Csv => writeln!(
                self.file,
                "window,{},,,,,{:.3},{:.3},{},{}",
                elapsed_ms, fps, lines_per_frame, status_updates, border_updates
            ),
        }
    }
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
            KeymapCommand::Doctor => run_keymap_doctor(),
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
            .is_some_and(|message| Instant::now() >= message.expires_at)
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

fn spawn_pane(
    shell: &str,
    title: String,
    pane_inner: Rect,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
) -> Result<PaneRuntime> {
    let state = Arc::new(PaneState {
        parser: Mutex::new(VtParser::new(
            pane_inner.height.max(MIN_PANE_ROWS),
            pane_inner.width.max(MIN_PANE_COLS),
            10_000,
        )),
        dirty: AtomicBool::new(true),
    });

    Ok(PaneRuntime {
        title: title.clone(),
        shell: shell.to_string(),
        process: Some(spawn_pane_process(
            shell,
            title,
            pane_inner,
            startup_deadline,
            user_input_seen,
            Arc::clone(&state),
        )?),
        state,
        closed: false,
        exit_code: None,
    })
}

fn spawn_pane_process(
    shell: &str,
    title: String,
    pane_inner: Rect,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
    state: Arc<PaneState>,
) -> Result<PaneProcess> {
    let pty_system = native_pty_system();
    let pty_pair = pty_system
        .openpty(PtySize {
            rows: pane_inner.height.max(MIN_PANE_ROWS),
            cols: pane_inner.width.max(MIN_PANE_COLS),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open pane PTY")?;

    let command = CommandBuilder::new(shell);
    let child = pty_pair
        .slave
        .spawn_command(command)
        .context("failed to spawn shell in pane")?;
    drop(pty_pair.slave);

    {
        let mut parser = state.parser.lock().expect("pane parser mutex poisoned");
        parser.screen_mut().set_size(
            pane_inner.height.max(MIN_PANE_ROWS),
            pane_inner.width.max(MIN_PANE_COLS),
        );
    }
    state.dirty.store(true, Ordering::Relaxed);

    let mut reader = pty_pair
        .master
        .try_clone_reader()
        .context("failed to clone pane PTY reader")?;
    let writer = pty_pair
        .master
        .take_writer()
        .context("failed to open pane PTY writer")?;

    let state_for_thread = Arc::clone(&state);
    let output_thread = thread::Builder::new()
        .name(format!("bmux-pane-output-{title}"))
        .spawn(move || -> Result<()> {
            let mut buffer = [0_u8; 8192];
            let mut pending = Vec::new();

            loop {
                let bytes_read = reader
                    .read(&mut buffer)
                    .context("failed reading pane PTY output")?;
                if bytes_read == 0 {
                    break;
                }

                pending.extend_from_slice(&buffer[..bytes_read]);
                let startup_guard_active =
                    !user_input_seen.load(Ordering::Relaxed) && Instant::now() < startup_deadline;

                let (output, dropped_exit_sequence) =
                    extract_filtered_output(&mut pending, startup_guard_active);

                if dropped_exit_sequence {
                    debug!("Dropped startup alt-screen exit sequence from pane output");
                }

                if output.is_empty() {
                    continue;
                }

                let mut parser = state_for_thread
                    .parser
                    .lock()
                    .expect("pane parser mutex poisoned");
                parser.process(&output);
                state_for_thread.dirty.store(true, Ordering::Relaxed);
            }

            Ok(())
        })
        .context("failed to spawn pane output thread")?;

    Ok(PaneProcess {
        master: pty_pair.master,
        writer,
        child,
        output_thread: Some(output_thread),
    })
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

fn process_input_events(
    input_rx: &Receiver<RuntimeAction>,
    panes: &mut [PaneRuntime],
    layout: &Layout,
    focused_pane: &mut usize,
    split_ratio: &mut f32,
    shutdown_requested: &Arc<AtomicBool>,
    force_redraw: &mut bool,
    exit_override: &mut Option<u8>,
    status_message: &mut Option<StatusMessage>,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        match input_rx.try_recv() {
            Ok(RuntimeAction::ForwardToPane(bytes)) => {
                if let Some(active_pane) = panes.get_mut(*focused_pane) {
                    if let Some(process) = active_pane.process.as_mut() {
                        process
                            .writer
                            .write_all(&bytes)
                            .and_then(|_| process.writer.flush())
                            .context("failed writing input to pane")?;
                    }
                }
            }
            Ok(action) => {
                match action {
                    RuntimeAction::Quit => {
                        shutdown_requested.store(true, Ordering::Relaxed);
                        *exit_override = Some(0);
                    }
                    RuntimeAction::FocusNext => {
                        *focused_pane = next_focusable_pane_index(panes, *focused_pane);
                    }
                    RuntimeAction::IncreaseSplit => {
                        *split_ratio = (*split_ratio + SPLIT_RATIO_STEP).clamp(0.2, 0.8);
                    }
                    RuntimeAction::DecreaseSplit => {
                        *split_ratio = (*split_ratio - SPLIT_RATIO_STEP).clamp(0.2, 0.8);
                    }
                    RuntimeAction::RestartFocusedPane => {
                        let pane_inner = if *focused_pane == 0 {
                            layout.left.inner()
                        } else {
                            layout.right.inner()
                        };
                        if let Some(pane) = panes.get_mut(*focused_pane) {
                            stop_pane_process(pane, true)?;
                            pane.process = Some(spawn_pane_process(
                                &pane.shell,
                                pane.title.clone(),
                                pane_inner,
                                startup_deadline,
                                Arc::clone(&user_input_seen),
                                Arc::clone(&pane.state),
                            )?);
                            pane.closed = false;
                            pane.exit_code = None;
                            pane.state.dirty.store(true, Ordering::Relaxed);
                            *status_message = Some(new_status_message(format!(
                                "pane '{}' restarted",
                                pane.title
                            )));
                        }
                    }
                    RuntimeAction::CloseFocusedPane => {
                        let running_count =
                            panes.iter().filter(|pane| pane_is_running(pane)).count();
                        if running_count <= 1 {
                            *status_message = Some(new_status_message(
                                "cannot close the last running pane".to_string(),
                            ));
                        } else if let Some(pane) = panes.get_mut(*focused_pane) {
                            let closed_title = pane.title.clone();
                            stop_pane_process(pane, true)?;
                            pane.closed = true;
                            pane.exit_code = None;
                            pane.state.dirty.store(true, Ordering::Relaxed);
                            *status_message =
                                Some(new_status_message(format!("pane '{closed_title}' closed")));
                        }

                        *focused_pane = next_focusable_pane_index(panes, *focused_pane);
                    }
                    RuntimeAction::ShowHelp => {
                        *status_message = Some(new_status_message(
                            "Ctrl-A: q quit | o focus | +/- resize | r restart | x close | ? help"
                                .to_string(),
                        ));
                    }
                    RuntimeAction::Eof => {
                        shutdown_requested.store(true, Ordering::Relaxed);
                        *exit_override = Some(0);
                    }
                    RuntimeAction::ForwardToPane(_) => unreachable!(),
                }

                *force_redraw = true;
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                shutdown_requested.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    Ok(())
}

fn refresh_exit_codes(panes: &mut [PaneRuntime]) -> Result<()> {
    for pane in panes.iter_mut() {
        let Some(process) = pane.process.as_mut() else {
            continue;
        };

        if let Some(status) = process
            .child
            .try_wait()
            .context("failed to poll pane shell status")?
        {
            pane.exit_code = Some(exit_code_from_u32(status.exit_code()));
            stop_pane_process(pane, false)?;
            pane.closed = false;
            pane.state.dirty.store(true, Ordering::Relaxed);
        }
    }

    Ok(())
}

fn stop_pane_process(pane: &mut PaneRuntime, kill: bool) -> Result<()> {
    if let Some(mut process) = pane.process.take() {
        if kill {
            let _ = process.child.kill();
        }

        let _ = process.child.wait();

        if let Some(output_thread) = process.output_thread.take() {
            match output_thread.join() {
                Ok(result) => result.context("PTY output thread failed")?,
                Err(_) => return Err(anyhow::anyhow!("PTY output thread panicked")),
            }
        }
    }

    Ok(())
}

fn pane_is_running(pane: &PaneRuntime) -> bool {
    pane.process.is_some()
}

fn any_running_panes(panes: &[PaneRuntime]) -> bool {
    panes.iter().any(pane_is_running)
}

fn first_running_pane_index(panes: &[PaneRuntime]) -> Option<usize> {
    panes.iter().position(pane_is_running)
}

fn next_focusable_pane_index(panes: &[PaneRuntime], current: usize) -> usize {
    if panes.is_empty() {
        return 0;
    }

    for offset in 1..=panes.len() {
        let index = (current + offset) % panes.len();
        if pane_is_running(&panes[index]) {
            return index;
        }
    }

    current.min(panes.len() - 1)
}

fn new_status_message(text: String) -> StatusMessage {
    StatusMessage {
        text,
        expires_at: Instant::now() + STATUS_MESSAGE_TTL,
    }
}

fn resize_panes(panes: &mut [PaneRuntime], layout: &Layout) -> Result<()> {
    for (pane, rect) in panes
        .iter_mut()
        .zip([layout.left.inner(), layout.right.inner()])
    {
        if let Some(process) = pane.process.as_mut() {
            process
                .master
                .resize(PtySize {
                    rows: rect.height.max(MIN_PANE_ROWS),
                    cols: rect.width.max(MIN_PANE_COLS),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("failed to resize pane PTY")?;
        }

        let mut parser = pane
            .state
            .parser
            .lock()
            .expect("pane parser mutex poisoned");
        parser.screen_mut().set_size(
            rect.height.max(MIN_PANE_ROWS),
            rect.width.max(MIN_PANE_COLS),
        );
        pane.state.dirty.store(true, Ordering::Relaxed);
    }

    Ok(())
}

fn render_frame(
    panes: &[PaneRuntime],
    layout: &Layout,
    cols: u16,
    rows: u16,
    shell_name: &str,
    cwd: &Path,
    focused_pane: usize,
    status_message: Option<&str>,
    full_redraw: bool,
    render_cache: &mut RenderCache,
    render_debug: &mut RenderDebugState,
) -> Result<()> {
    let debug_snapshot = render_debug.snapshot();
    let status_suffix = match (status_message, debug_snapshot) {
        (Some(message), Some(debug)) => Some(format!("{message} | {debug}")),
        (Some(message), None) => Some(message.to_string()),
        (None, Some(debug)) => Some(debug.to_string()),
        (None, None) => None,
    };

    let status_line = build_status_line(
        shell_name,
        cwd,
        cols,
        rows,
        focused_pane,
        status_suffix.as_deref(),
    );
    let left_data = collect_pane_render_data(&panes[0], layout.left, focused_pane == 0);
    let right_data = collect_pane_render_data(&panes[1], layout.right, focused_pane == 1);
    let pane_data = [left_data, right_data];

    let mut stdout = io::stdout();
    write!(stdout, "\x1b[?25l").context("failed hiding cursor")?;

    let status_changed =
        full_redraw || !render_cache.initialized || render_cache.status_line != status_line;
    if status_changed {
        write_status_line(&status_line, cols).context("failed drawing status line")?;
        render_cache.status_line = status_line;
    }

    let border_changed = full_redraw
        || !render_cache.initialized
        || render_cache.focused_pane != focused_pane
        || render_cache.pane_rects != [pane_data[0].rect, pane_data[1].rect]
        || render_cache.pane_titles != [pane_data[0].title.clone(), pane_data[1].title.clone()];

    let mut changed_line_count = 0_usize;
    for pane_index in 0..2 {
        changed_line_count += draw_changed_lines(
            &mut stdout,
            &pane_data[pane_index],
            &render_cache.pane_lines[pane_index],
            full_redraw || border_changed,
        )?;
    }

    if border_changed || changed_line_count > 0 {
        draw_rect_border(
            &mut stdout,
            pane_data[0].rect,
            if focused_pane == 0 {
                "\x1b[36m"
            } else {
                "\x1b[90m"
            },
            &pane_data[0].title,
        )?;
        draw_rect_border(
            &mut stdout,
            pane_data[1].rect,
            if focused_pane == 1 {
                "\x1b[36m"
            } else {
                "\x1b[90m"
            },
            &pane_data[1].title,
        )?;
    }

    let focused_rect = if focused_pane == 0 {
        layout.left.inner()
    } else {
        layout.right.inner()
    };

    let cursor_pos = {
        let parser = panes[focused_pane]
            .state
            .parser
            .lock()
            .expect("pane parser mutex poisoned");
        parser.screen().cursor_position()
    };

    let cursor_row = focused_rect
        .y
        .saturating_add(cursor_pos.0.min(focused_rect.height.saturating_sub(1)));
    let cursor_col = focused_rect
        .x
        .saturating_add(cursor_pos.1.min(focused_rect.width.saturating_sub(1)));

    write!(stdout, "\x1b[?25h\x1b[{cursor_row};{cursor_col}H").context("failed setting cursor")?;
    stdout.flush().context("failed flushing rendered frame")?;

    render_cache.initialized = true;
    render_cache.focused_pane = focused_pane;
    render_cache.pane_rects = [pane_data[0].rect, pane_data[1].rect];
    render_cache.pane_titles = [pane_data[0].title.clone(), pane_data[1].title.clone()];
    render_cache.pane_lines = [pane_data[0].lines.clone(), pane_data[1].lines.clone()];
    render_debug.record_frame(changed_line_count, status_changed, border_changed);

    Ok(())
}

fn collect_pane_render_data(pane: &PaneRuntime, rect: Rect, focused: bool) -> PaneRenderData {
    let inner = rect.inner();
    let title = if pane.closed {
        format!(" {} [closed] ", pane.title)
    } else if let Some(code) = pane.exit_code {
        format!(" {} [exited {code}] ", pane.title)
    } else {
        format!(" {}{} ", pane.title, if focused { " *" } else { "" })
    };

    let mut lines = Vec::new();

    if inner.width == 0 || inner.height == 0 {
        return PaneRenderData { rect, title, lines };
    }

    if pane.closed {
        let message = "pane closed | Ctrl-A r restart".to_string();
        lines.push(pad_or_truncate(&message, usize::from(inner.width)).into_bytes());
        for _ in 1..inner.height {
            lines.push(vec![b' '; usize::from(inner.width)]);
        }
        return PaneRenderData { rect, title, lines };
    }

    if let Some(code) = pane.exit_code {
        let message = format!("pane exited ({code}) | Ctrl-A r restart");
        lines.push(pad_or_truncate(&message, usize::from(inner.width)).into_bytes());
        for _ in 1..inner.height {
            lines.push(vec![b' '; usize::from(inner.width)]);
        }
        return PaneRenderData { rect, title, lines };
    }

    let parser = pane
        .state
        .parser
        .lock()
        .expect("pane parser mutex poisoned");
    let screen = parser.screen();
    for row_index in 0..inner.height {
        lines.push(render_screen_row(screen, row_index, inner.width));
    }

    PaneRenderData { rect, title, lines }
}

fn render_screen_row(screen: &Screen, row: u16, width: u16) -> Vec<u8> {
    let mut output = Vec::with_capacity(usize::from(width) * 4);
    let mut current_style = CellStyle::default();
    let mut col = 0_u16;

    while col < width {
        let remaining = width - col;
        let Some(cell) = screen.cell(row, col) else {
            push_style_diff(&mut output, current_style, CellStyle::default());
            output.push(b' ');
            current_style = CellStyle::default();
            col += 1;
            continue;
        };

        if cell.is_wide_continuation() {
            col += 1;
            continue;
        }

        let style = CellStyle {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        };

        push_style_diff(&mut output, current_style, style);
        current_style = style;

        if cell.has_contents() {
            if cell.is_wide() && remaining < 2 {
                output.push(b' ');
                col += 1;
            } else {
                output.extend_from_slice(cell.contents().as_bytes());
                col += if cell.is_wide() { 2 } else { 1 };
            }
        } else {
            output.push(b' ');
            col += 1;
        }
    }

    push_style_diff(&mut output, current_style, CellStyle::default());
    output
}

fn push_style_diff(output: &mut Vec<u8>, previous: CellStyle, next: CellStyle) {
    if previous == next {
        return;
    }

    output.extend_from_slice(b"\x1b[0m");

    let mut codes = Vec::new();
    if next.bold {
        codes.push("1".to_string());
    }
    if next.dim {
        codes.push("2".to_string());
    }
    if next.italic {
        codes.push("3".to_string());
    }
    if next.underline {
        codes.push("4".to_string());
    }
    if next.inverse {
        codes.push("7".to_string());
    }

    push_color_code(&mut codes, next.fg, true);
    push_color_code(&mut codes, next.bg, false);

    if codes.is_empty() {
        return;
    }

    output.extend_from_slice(b"\x1b[");
    output.extend_from_slice(codes.join(";").as_bytes());
    output.push(b'm');
}

fn push_color_code(codes: &mut Vec<String>, color: Color, foreground: bool) {
    match color {
        Color::Default => {
            codes.push(if foreground {
                "39".to_string()
            } else {
                "49".to_string()
            });
        }
        Color::Idx(value) => {
            let base = if foreground { 30_u8 } else { 40_u8 };
            let bright_base = if foreground { 90_u8 } else { 100_u8 };

            if value <= 7 {
                codes.push((base + value).to_string());
            } else if value <= 15 {
                codes.push((bright_base + (value - 8)).to_string());
            } else {
                if foreground {
                    codes.push("38".to_string());
                } else {
                    codes.push("48".to_string());
                }
                codes.push("5".to_string());
                codes.push(value.to_string());
            }
        }
        Color::Rgb(red, green, blue) => {
            if foreground {
                codes.push("38".to_string());
            } else {
                codes.push("48".to_string());
            }
            codes.push("2".to_string());
            codes.push(red.to_string());
            codes.push(green.to_string());
            codes.push(blue.to_string());
        }
    }
}

fn draw_changed_lines(
    stdout: &mut io::Stdout,
    pane: &PaneRenderData,
    previous: &[Vec<u8>],
    force: bool,
) -> Result<usize> {
    let inner = pane.rect.inner();
    if inner.width == 0 || inner.height == 0 {
        return Ok(0);
    }

    let mut changed = 0_usize;

    for (row_index, line) in pane.lines.iter().enumerate() {
        let line_changed = force || previous.get(row_index) != Some(line);
        if !line_changed {
            continue;
        }

        let y = inner
            .y
            .saturating_add(u16::try_from(row_index).unwrap_or(0));
        write_at_bytes(stdout, inner.x, y, line)?;
        changed += 1;
    }

    Ok(changed)
}

fn draw_rect_border(stdout: &mut io::Stdout, rect: Rect, color: &str, title: &str) -> Result<()> {
    if rect.width < 2 || rect.height < 2 {
        return Ok(());
    }

    let inner_width = usize::from(rect.width.saturating_sub(2));
    let mut title_inner = fit_to_width(title, inner_width);
    let title_w = UnicodeWidthStr::width(title_inner.as_str());
    if title_w < inner_width {
        title_inner.push_str(&"-".repeat(inner_width - title_w));
    }
    let top = format!("+{title_inner}+");
    let bottom = top.clone();
    let right_x = rect.x.saturating_add(rect.width.saturating_sub(1));

    write_at(stdout, rect.x, rect.y, &format!("{color}{top}\x1b[0m"))?;
    for offset in 1..rect.height.saturating_sub(1) {
        let y = rect.y.saturating_add(offset);
        write_at(stdout, rect.x, y, &format!("{color}|\x1b[0m"))?;
        write_at(stdout, right_x, y, &format!("{color}|\x1b[0m"))?;
    }
    write_at(
        stdout,
        rect.x,
        rect.y.saturating_add(rect.height.saturating_sub(1)),
        &format!("{color}{bottom}\x1b[0m"),
    )?;

    Ok(())
}

fn write_at(stdout: &mut io::Stdout, x: u16, y: u16, text: &str) -> Result<()> {
    write!(stdout, "\x1b[{y};{x}H{text}").context("failed writing terminal content")
}

fn write_at_bytes(stdout: &mut io::Stdout, x: u16, y: u16, bytes: &[u8]) -> Result<()> {
    write!(stdout, "\x1b[{y};{x}H").context("failed moving cursor for bytes")?;
    stdout
        .write_all(bytes)
        .context("failed writing terminal byte content")?;
    write!(stdout, "\x1b[0m").context("failed resetting terminal attributes")
}

fn pad_or_truncate(text: &str, width: usize) -> String {
    let mut rendered = fit_to_width(text, width);
    let current_width = UnicodeWidthStr::width(rendered.as_str());
    if current_width < width {
        rendered.push_str(&" ".repeat(width.saturating_sub(current_width)));
    }

    rendered
}

fn fit_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let mut rendered = String::new();
    let mut used = 0_usize;
    for character in text.chars() {
        let char_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if char_width == 0 {
            continue;
        }

        if used + char_width > width {
            break;
        }

        rendered.push(character);
        used += char_width;
    }

    rendered
}

#[cfg(test)]
mod tests {
    use super::{pad_or_truncate, render_screen_row};
    use unicode_width::UnicodeWidthStr;
    use vt100::Parser;

    #[test]
    fn pad_or_truncate_handles_wide_characters() {
        let rendered = pad_or_truncate("a界b", 4);
        assert_eq!(UnicodeWidthStr::width(rendered.as_str()), 4);
    }

    #[test]
    fn row_renderer_keeps_colors_without_erase_controls() {
        let mut parser = Parser::new(2, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m text\x1b[K");

        let row = render_screen_row(parser.screen(), 0, 20);
        let rendered = String::from_utf8(row).expect("valid utf8 row output");

        assert!(rendered.contains("red"));
        assert!(rendered.contains("\x1b["));
        assert!(!rendered.contains("\x1b[K"));
        assert!(!rendered.contains("\x1b[2K"));
    }
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

fn run_keymap_doctor() -> Result<u8> {
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
