use crate::cli::Cli;
use crate::pane::{compute_vertical_layout, Layout, Rect};
use crate::pty::{extract_filtered_output, STARTUP_ALT_SCREEN_GUARD_DURATION};
use crate::status::{build_status_line, write_status_line};
use crate::terminal::TerminalGuard;
use anyhow::{Context, Result};
use clap::Parser;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
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

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const STATUS_REDRAW_INTERVAL: Duration = Duration::from_millis(200);
const EXIT_KEY_PREFIX: u8 = 0x01;
const SPLIT_RATIO_STEP: f32 = 0.05;
const MIN_PANE_ROWS: u16 = 2;
const MIN_PANE_COLS: u16 = 2;

#[derive(Debug, Clone, Copy)]
enum InputCommand {
    Quit,
    FocusNext,
    IncreaseSplit,
    DecreaseSplit,
}

enum InputEvent {
    Data(Vec<u8>),
    Command(InputCommand),
    Eof,
}

struct PaneState {
    parser: Mutex<VtParser>,
    dirty: AtomicBool,
}

struct PaneRuntime {
    title: String,
    state: Arc<PaneState>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send>,
    output_thread: Option<thread::JoinHandle<Result<()>>>,
    exit_code: Option<u8>,
}

#[derive(Default)]
struct RenderCache {
    initialized: bool,
    status_line: String,
    pane_rects: [Rect; 2],
    pane_titles: [String; 2],
    focused_pane: usize,
    pane_lines: [Vec<String>; 2],
}

struct PaneRenderData {
    rect: Rect,
    title: String,
    lines: Vec<String>,
}

struct RenderDebugState {
    enabled: bool,
    window_start: Instant,
    frames: u32,
    changed_lines: usize,
    status_updates: u32,
    border_updates: u32,
    snapshot: String,
}

impl RenderDebugState {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            window_start: Instant::now(),
            frames: 0,
            changed_lines: 0,
            status_updates: 0,
            border_updates: 0,
            snapshot: String::new(),
        }
    }

    fn record_frame(&mut self, changed_lines: usize, status_updated: bool, border_updated: bool) {
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

pub(crate) fn run() -> Result<u8> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let shell = resolve_shell(cli.shell);
    debug!("Starting bmux runtime");
    debug!("Launching shell: {shell}");

    run_two_pane_runtime(&shell, !cli.no_alt_screen, cli.debug_render)
}

fn run_two_pane_runtime(shell: &str, use_alt_screen: bool, debug_render: bool) -> Result<u8> {
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

    let (input_tx, input_rx) = mpsc::channel::<InputEvent>();
    let input_thread = spawn_input_thread(
        input_tx,
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
    let mut render_cache = RenderCache::default();
    let mut render_debug = RenderDebugState::new(debug_render);

    let exit_code = loop {
        process_input_events(
            &input_rx,
            &mut panes,
            &mut focused_pane,
            &mut split_ratio,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
        )?;

        if shutdown_requested.load(Ordering::Relaxed) && !kill_sent {
            debug!("Terminating pane shells");
            for pane in &mut panes {
                let _ = pane.child.kill();
            }
            kill_sent = true;
        }

        if refresh_exit_codes(&mut panes)? {
            break panes.first().and_then(|pane| pane.exit_code).unwrap_or(0);
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
        let _ = pane.child.wait();
        if let Some(output_thread) = pane.output_thread.take() {
            match output_thread.join() {
                Ok(result) => result.context("PTY output thread failed")?,
                Err(_) => return Err(anyhow::anyhow!("PTY output thread panicked")),
            }
        }
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

    let mut reader = pty_pair
        .master
        .try_clone_reader()
        .context("failed to clone pane PTY reader")?;
    let writer = pty_pair
        .master
        .take_writer()
        .context("failed to open pane PTY writer")?;

    let state = Arc::new(PaneState {
        parser: Mutex::new(VtParser::new(
            pane_inner.height.max(MIN_PANE_ROWS),
            pane_inner.width.max(MIN_PANE_COLS),
            10_000,
        )),
        dirty: AtomicBool::new(true),
    });

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

    Ok(PaneRuntime {
        title,
        state,
        master: pty_pair.master,
        writer,
        child,
        output_thread: Some(output_thread),
        exit_code: None,
    })
}

fn spawn_input_thread(
    input_tx: Sender<InputEvent>,
    user_input_seen: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<Result<()>>> {
    let input_thread = thread::Builder::new()
        .name("bmux-pty-input".to_string())
        .spawn(move || -> Result<()> {
            let mut stdin = io::stdin().lock();
            let mut buffer = [0_u8; 8192];
            let mut prefix_pending = false;

            loop {
                if shutdown_requested.load(Ordering::Relaxed) {
                    break;
                }

                let bytes_read = stdin
                    .read(&mut buffer)
                    .context("failed reading terminal input")?;

                if bytes_read == 0 {
                    let _ = input_tx.send(InputEvent::Eof);
                    break;
                }

                user_input_seen.store(true, Ordering::Relaxed);

                let mut forwarded = Vec::with_capacity(bytes_read + 1);
                for byte in &buffer[..bytes_read] {
                    if prefix_pending {
                        prefix_pending = false;
                        match *byte {
                            b'q' | b'Q' => {
                                let _ = input_tx.send(InputEvent::Command(InputCommand::Quit));
                                continue;
                            }
                            b'o' | b'O' => {
                                let _ = input_tx.send(InputEvent::Command(InputCommand::FocusNext));
                                continue;
                            }
                            b'+' => {
                                let _ =
                                    input_tx.send(InputEvent::Command(InputCommand::IncreaseSplit));
                                continue;
                            }
                            b'-' => {
                                let _ =
                                    input_tx.send(InputEvent::Command(InputCommand::DecreaseSplit));
                                continue;
                            }
                            EXIT_KEY_PREFIX => {
                                forwarded.push(EXIT_KEY_PREFIX);
                                continue;
                            }
                            _ => {
                                forwarded.push(EXIT_KEY_PREFIX);
                                forwarded.push(*byte);
                                continue;
                            }
                        }
                    }

                    if *byte == EXIT_KEY_PREFIX {
                        prefix_pending = true;
                        continue;
                    }

                    forwarded.push(*byte);
                }

                if !forwarded.is_empty() {
                    let _ = input_tx.send(InputEvent::Data(forwarded));
                }
            }

            Ok(())
        })
        .context("failed to spawn PTY input thread")?;

    Ok(input_thread)
}

fn process_input_events(
    input_rx: &Receiver<InputEvent>,
    panes: &mut [PaneRuntime],
    focused_pane: &mut usize,
    split_ratio: &mut f32,
    shutdown_requested: &Arc<AtomicBool>,
    force_redraw: &mut bool,
    exit_override: &mut Option<u8>,
) -> Result<()> {
    loop {
        match input_rx.try_recv() {
            Ok(InputEvent::Data(bytes)) => {
                if let Some(pane) = panes.get_mut(*focused_pane) {
                    pane.writer
                        .write_all(&bytes)
                        .and_then(|_| pane.writer.flush())
                        .context("failed writing input to pane")?;
                }
            }
            Ok(InputEvent::Command(command)) => {
                match command {
                    InputCommand::Quit => shutdown_requested.store(true, Ordering::Relaxed),
                    InputCommand::FocusNext => {
                        *focused_pane = (*focused_pane + 1) % panes.len().max(1);
                    }
                    InputCommand::IncreaseSplit => {
                        *split_ratio = (*split_ratio + SPLIT_RATIO_STEP).clamp(0.2, 0.8);
                    }
                    InputCommand::DecreaseSplit => {
                        *split_ratio = (*split_ratio - SPLIT_RATIO_STEP).clamp(0.2, 0.8);
                    }
                }

                *force_redraw = true;

                if matches!(command, InputCommand::Quit) {
                    *exit_override = Some(0);
                }
            }
            Ok(InputEvent::Eof) => {
                shutdown_requested.store(true, Ordering::Relaxed);
                *force_redraw = true;
                *exit_override = Some(0);
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

fn refresh_exit_codes(panes: &mut [PaneRuntime]) -> Result<bool> {
    for pane in panes.iter_mut() {
        if pane.exit_code.is_some() {
            continue;
        }

        if let Some(status) = pane
            .child
            .try_wait()
            .context("failed to poll pane shell status")?
        {
            pane.exit_code = Some(exit_code_from_u32(status.exit_code()));
        }
    }

    Ok(panes.iter().all(|pane| pane.exit_code.is_some()))
}

fn resize_panes(panes: &mut [PaneRuntime], layout: &Layout) -> Result<()> {
    for (pane, rect) in panes
        .iter_mut()
        .zip([layout.left.inner(), layout.right.inner()])
    {
        pane.master
            .resize(PtySize {
                rows: rect.height.max(MIN_PANE_ROWS),
                cols: rect.width.max(MIN_PANE_COLS),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize pane PTY")?;

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
    full_redraw: bool,
    render_cache: &mut RenderCache,
    render_debug: &mut RenderDebugState,
) -> Result<()> {
    let status_line = build_status_line(
        shell_name,
        cwd,
        cols,
        rows,
        focused_pane,
        render_debug.snapshot(),
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

    if border_changed {
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

    let mut changed_line_count = 0_usize;
    for pane_index in 0..2 {
        changed_line_count += draw_changed_lines(
            &mut stdout,
            &pane_data[pane_index],
            &render_cache.pane_lines[pane_index],
            full_redraw || border_changed,
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
    let title = if let Some(code) = pane.exit_code {
        format!(" {} [exited {code}] ", pane.title)
    } else {
        format!(" {}{} ", pane.title, if focused { " *" } else { "" })
    };

    let mut lines = Vec::new();

    if inner.width == 0 || inner.height == 0 {
        return PaneRenderData { rect, title, lines };
    }

    if let Some(code) = pane.exit_code {
        let message = format!("[{} exited: {code}]", pane.title);
        lines.push(pad_or_truncate(&message, usize::from(inner.width)));
        for _ in 1..inner.height {
            lines.push(" ".repeat(usize::from(inner.width)));
        }
        return PaneRenderData { rect, title, lines };
    }

    let parser = pane
        .state
        .parser
        .lock()
        .expect("pane parser mutex poisoned");
    let screen = parser.screen();
    for (row_index, row_text) in screen.rows(0, inner.width).enumerate() {
        if row_index >= usize::from(inner.height) {
            break;
        }
        lines.push(pad_or_truncate(&row_text, usize::from(inner.width)));
    }

    while lines.len() < usize::from(inner.height) {
        lines.push(" ".repeat(usize::from(inner.width)));
    }

    PaneRenderData { rect, title, lines }
}

fn draw_changed_lines(
    stdout: &mut io::Stdout,
    pane: &PaneRenderData,
    previous: &[String],
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
        write_at(stdout, inner.x, y, line)?;
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
    let middle = format!(
        "|{}|",
        " ".repeat(usize::from(rect.width.saturating_sub(2)))
    );
    let bottom = top.clone();

    write_at(stdout, rect.x, rect.y, &format!("{color}{top}\x1b[0m"))?;
    for offset in 1..rect.height.saturating_sub(1) {
        write_at(
            stdout,
            rect.x,
            rect.y.saturating_add(offset),
            &format!("{color}{middle}\x1b[0m"),
        )?;
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
    use super::pad_or_truncate;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn pad_or_truncate_handles_wide_characters() {
        let rendered = pad_or_truncate("a界b", 4);
        assert_eq!(UnicodeWidthStr::width(rendered.as_str()), 4);
    }
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
