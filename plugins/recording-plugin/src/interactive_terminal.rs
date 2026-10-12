//! Undecorated recorded-execution frontend. The server owns parsing and query replies.
use std::io::{IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use bmux_client::BmuxClient;
use bmux_keyboard::encode::{KeyEncodingModes, encode_key_with_modes};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4 as pane;
use bmux_recording_plugin_api::{
    recording_process_presentation_v1 as presentation, recording_process_v4 as process,
};
use bmux_terminal_grid::{GridLimits, GridSnapshot, TerminalGridStream};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use uuid::Uuid;

const MAX_INPUT: usize = 16_384;
const MAX_SNAPSHOT: usize = 4 * 1024 * 1024;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
// Give already-buffered output a bounded presentation grace, never wait for
// arbitrary surviving slave holders while owning the caller's raw terminal.
const POST_EXIT_DRAIN_GRACE: Duration = Duration::from_millis(250);

type InputModes = bmux_terminal_grid::ProtocolState;

/// Every control emitted here is frontend-owned. Original application escapes
/// never reach stdout: retained grids are repainted instead.
struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "interactive recording requires terminal stdin and stdout"
        );
        enable_raw_mode().context("entering interactive recording raw mode")?;
        let mut guard = Self { active: true };
        let result = (|| {
            let mut out = std::io::stdout().lock();
            out.write_all(b"\x1b[?1049h\x1b[?2004h\x1b[?1004h\x1b[?1003h\x1b[?1006h\x1b[?7l")?;
            out.flush()
        })();
        if let Err(error) = result {
            return combine_restore::<Self>(Err(error.into()), guard.restore());
        }
        Ok(guard)
    }

    fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let display = (|| {
            let mut out = std::io::stdout().lock();
            out.write_all(b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?7h\x1b[0m\x1b[?25h\x1b[?1049l")?;
            out.flush()
        })();
        let raw = disable_raw_mode();
        self.active = display.is_err() || raw.is_err();
        match (display, raw) {
            (Ok(()), Ok(())) => Ok(()),
            (display, raw) => {
                bail!("terminal restoration failed: display={display:?}; raw={raw:?}")
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.restore(); // Explicit restore above reports failure; drop is best effort only.
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InteractiveFailureKind {
    Restoration,
    Disconnect,
    Transport,
    Interrupted,
    Unsupported,
    Operation,
}

impl std::fmt::Display for InteractiveFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "interactive failure: {self:?}")
    }
}

#[derive(Debug)]
struct RestorationObserved(bool);

impl std::fmt::Display for RestorationObserved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "terminal_restored={}", self.0)
    }
}

pub(crate) fn failure_kind(error: &anyhow::Error) -> InteractiveFailureKind {
    error
        .downcast_ref::<InteractiveFailureKind>()
        .copied()
        .unwrap_or(InteractiveFailureKind::Operation)
}

/// None means restoration was not observed; never turn it into success.
pub(crate) fn terminal_restored(error: &anyhow::Error) -> Option<bool> {
    error
        .downcast_ref::<RestorationObserved>()
        .map(|observed| observed.0)
}

fn combine_restore<T>(operation: Result<T>, restoration: Result<()>) -> Result<T> {
    match (operation, restoration) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => {
            let kind = failure_kind(&error);
            Err(error.context(kind).context(RestorationObserved(true)))
        }
        (Ok(_), Err(error)) => Err(error
            .context(InteractiveFailureKind::Restoration)
            .context(RestorationObserved(false))),
        (Err(operation), Err(restoration)) => Err(operation
            .context(format!("also failed terminal restoration: {restoration:#}"))
            .context(InteractiveFailureKind::Restoration)
            .context(RestorationObserved(false))),
    }
}

fn viewport() -> Result<(u16, u16)> {
    let (cols, rows) = crossterm::terminal::size().context("reading interactive viewport")?;
    ensure!(
        cols > 0 && rows > 0 && cols <= 500 && rows <= 300,
        "interactive viewport outside supported 1..500 by 1..300 bounds"
    );
    Ok((cols, rows))
}

fn render<W: Write>(writer: &mut W, encoded: &[u8], cols: u16, rows: u16) -> Result<InputModes> {
    ensure!(
        encoded.len() <= MAX_SNAPSHOT,
        "interactive grid snapshot exceeds budget"
    );
    let snapshot: GridSnapshot =
        serde_json::from_slice(encoded).context("decoding authoritative interactive grid")?;
    ensure!(
        snapshot
            .rows
            .iter()
            .flat_map(|row| &row.runs)
            .all(|run| !run.text.chars().any(char::is_control)),
        "interactive grid contains control bytes instead of display text"
    );
    let modes = modes_from_protocol(snapshot.protocol)?;
    let stream = TerminalGridStream::from_snapshot(&snapshot, GridLimits::default())
        .context("validating authoritative interactive grid")?;
    ensure!(
        stream.grid().width() == usize::from(cols) && stream.grid().height() == usize::from(rows),
        "interactive grid geometry differs from acknowledged viewport"
    );
    let frame = repaint(stream.grid())?;
    ensure!(
        frame.len() <= MAX_FRAME,
        "interactive repaint exceeds frame budget"
    );
    writer
        .write_all(&frame)
        .context("writing interactive grid repaint")?;
    writer
        .flush()
        .context("committing interactive grid repaint")?;
    Ok(modes)
}

fn repaint(grid: &bmux_terminal_grid::TerminalGrid) -> Result<Vec<u8>> {
    use crossterm::style::{
        Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };
    fn color(value: bmux_terminal_grid::Color) -> Color {
        match value {
            bmux_terminal_grid::Color::Indexed(index) => Color::AnsiValue(index),
            bmux_terminal_grid::Color::Rgb { r, g, b } => Color::Rgb { r, g, b },
        }
    }
    let mut frame = b"\x1b[?25l\x1b[0m\x1b[2J".to_vec();
    for (row_index, row) in grid.viewport_rows().iter().enumerate() {
        let row_position = u16::try_from(row_index)?;
        crossterm::queue!(frame, crossterm::cursor::MoveTo(0, row_position))?;
        let mut current_style = None;
        for cell in row.cells().iter().take(grid.width()) {
            // A wide grapheme already advances two display columns. Never emit
            // a space for its continuation, which would shift every later cell.
            if cell.is_wide_continuation() {
                continue;
            }
            let style = grid.palette().get(cell.style());
            if current_style != Some(style) {
                crossterm::queue!(frame, SetAttribute(Attribute::Reset), ResetColor)?;
                for (enabled, attribute) in [
                    (style.bold, Attribute::Bold),
                    (style.dim, Attribute::Dim),
                    (style.italic, Attribute::Italic),
                    (style.underline, Attribute::Underlined),
                    (style.inverse, Attribute::Reverse),
                    (style.strike, Attribute::CrossedOut),
                ] {
                    if enabled {
                        crossterm::queue!(frame, SetAttribute(attribute))?;
                    }
                }
                if let Some(fg) = style.fg {
                    crossterm::queue!(frame, SetForegroundColor(color(fg)))?;
                }
                if let Some(bg) = style.bg {
                    crossterm::queue!(frame, SetBackgroundColor(color(bg)))?;
                }
                current_style = Some(style);
            }
            frame.extend_from_slice(if cell.text().is_empty() {
                b" "
            } else {
                cell.text().as_bytes()
            });
            ensure!(
                frame.len() <= MAX_FRAME,
                "interactive repaint exceeds budget"
            );
        }
    }
    let cursor = grid.cursor();
    let cursor_col = u16::try_from(cursor.col)?;
    let cursor_row = u16::try_from(cursor.row)?;
    crossterm::queue!(
        frame,
        SetAttribute(Attribute::Reset),
        ResetColor,
        crossterm::cursor::MoveTo(cursor_col, cursor_row)
    )?;
    frame.extend_from_slice(if cursor.visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    Ok(frame)
}

fn modes_from_protocol(protocol: InputModes) -> Result<InputModes> {
    ensure!(
        !protocol.mouse_utf8 && !protocol.mouse_urxvt,
        "unsupported application mouse encoding; execution retained"
    );
    Ok(protocol)
}

fn encode_input(event: Event, modes: InputModes) -> Result<Option<Vec<u8>>> {
    let bytes = match event {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Release {
                return Ok(None);
            }
            ensure!(
                !(key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('z' | 'Z'))),
                "outer terminal suspension is unsupported; execution retained for recovery"
            );
            let stroke = bmux_keyboard::crossterm::crossterm_key_event_to_stroke(&key)
                .context("unsupported interactive key; input suspended")?;
            encode_key_with_modes(
                &stroke,
                false,
                KeyEncodingModes {
                    application_cursor: modes.application_cursor,
                    application_keypad: modes.application_keypad,
                },
            )
            .context("interactive key cannot be encoded; input suspended")?
        }
        Event::Paste(text) => {
            ensure!(
                text.len() <= MAX_INPUT - 12,
                "interactive paste exceeds input budget"
            );
            if modes.bracketed_paste {
                [
                    b"\x1b[200~".as_slice(),
                    text.as_bytes(),
                    b"\x1b[201~".as_slice(),
                ]
                .concat()
            } else {
                text.into_bytes()
            }
        }
        Event::Mouse(mouse) => return encode_mouse(mouse, modes),
        Event::FocusGained | Event::FocusLost | Event::Resize(_, _) => return Ok(None),
    };
    ensure!(bytes.len() <= MAX_INPUT, "interactive input exceeds budget");
    Ok(Some(bytes))
}

fn encode_mouse(mouse: MouseEvent, modes: InputModes) -> Result<Option<Vec<u8>>> {
    use bmux_terminal_grid::MouseProtocolMode;
    use crossterm::event::MouseButton;
    let tracking = modes.mouse_mode();
    if tracking == MouseProtocolMode::None {
        return Ok(None);
    }
    let (mut code, release) = match mouse.kind {
        MouseEventKind::Down(button)
        | MouseEventKind::Up(button)
        | MouseEventKind::Drag(button) => {
            let button = match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
            };
            let dragging = matches!(mouse.kind, MouseEventKind::Drag(_));
            if tracking == MouseProtocolMode::Press
                && (dragging || matches!(mouse.kind, MouseEventKind::Up(_)))
            {
                return Ok(None);
            }
            if dragging
                && !matches!(
                    tracking,
                    MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
                )
            {
                return Ok(None);
            }
            (
                button + if dragging { 32 } else { 0 },
                matches!(mouse.kind, MouseEventKind::Up(_)),
            )
        }
        MouseEventKind::Moved => {
            if tracking != MouseProtocolMode::AnyMotion {
                return Ok(None);
            }
            (35, false)
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let x = u32::from(mouse.column) + 1;
    let y = u32::from(mouse.row) + 1;
    if modes.mouse_sgr {
        Ok(Some(
            format!("\x1b[<{};{x};{y}{}", code, if release { 'm' } else { 'M' }).into_bytes(),
        ))
    } else {
        ensure!(
            x <= 223 && y <= 223,
            "legacy mouse coordinates unrepresentable; input suspended"
        );
        if release {
            code = 3;
        }
        Ok(Some(vec![
            27,
            b'[',
            b'M',
            u8::try_from(code + 32)?,
            u8::try_from(x + 32)?,
            u8::try_from(y + 32)?,
        ]))
    }
}

/// Attach only to the identity created by recording start. Neither terminal
/// disconnect nor a frontend error grants process-termination authority.
pub(crate) async fn interact(
    client: &mut BmuxClient,
    run_id: Uuid,
    execution: process::ExecutionRef,
) -> Result<process::RunStatus> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut suspend =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP))?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut guard = TerminalGuard::enter()?;
    let operation = tokio::select! {
        result = interact_loop(client, run_id, execution) => result,
        _ = interrupt.recv() => Err(anyhow::anyhow!("interactive frontend interrupted; execution retained, pending mutations must be reconciled").context(InteractiveFailureKind::Interrupted)),
        _ = terminate.recv() => Err(anyhow::anyhow!("interactive frontend terminated; execution retained, pending mutations must be reconciled").context(InteractiveFailureKind::Interrupted)),
        _ = suspend.recv() => Err(anyhow::anyhow!("outer terminal suspension unsupported; execution retained for recovery").context(InteractiveFailureKind::Unsupported)),
        _ = hangup.recv() => Err(anyhow::anyhow!("interactive terminal disconnected; execution retained for recovery").context(InteractiveFailureKind::Disconnect)),
    };
    combine_restore(operation, guard.restore())
}

async fn recorded_resize(
    client: &mut BmuxClient,
    run_id: Uuid,
    dimensions: (u16, u16),
) -> Result<()> {
    ensure!(
        dimensions.0 > 0 && dimensions.1 > 0 && dimensions.0 <= 500 && dimensions.1 <= 300,
        "interactive resize outside supported bounds"
    );
    presentation::client::resize(client, run_id,
        bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalCaptureFormat::OrderedEventsV1,
        dimensions.0, dimensions.1)
        .await.context("recorded resize outcome uncertain; no automatic retry; execution retained")
        .context(InteractiveFailureKind::Transport)?
        .map_err(|error| anyhow::anyhow!("recorded resize rejected; input suspended: {error:?}"))
}

fn poll_input() -> Result<Option<Event>> {
    if crossterm::event::poll(Duration::ZERO)
        .context("polling interactive terminal")
        .context(InteractiveFailureKind::Disconnect)?
    {
        Ok(Some(
            crossterm::event::read()
                .context("reading interactive terminal")
                .context(InteractiveFailureKind::Disconnect)?,
        ))
    } else {
        Ok(None)
    }
}

async fn dispatch_input(
    client: &mut BmuxClient,
    run_id: Uuid,
    event: Event,
    modes: InputModes,
) -> Result<()> {
    if let Some(data) = encode_input(event, modes)? {
        let mutation_id = Uuid::new_v4();
        process::client::send(&mut *client, run_id, data, mutation_id)
            .await
            .with_context(|| format!("interactive input outcome uncertain; reconcile mutation_id={mutation_id}"))
            .context(InteractiveFailureKind::Transport)?
            .map_err(|error| anyhow::anyhow!("interactive input rejected; mutation_id={mutation_id}; input suspended: {error:?}"))?;
    }
    Ok(())
}

// Keep authoritative status, presentation and input admission in one ordered
// loop; separating the transitions would obscure the post-exit input fence.
#[allow(clippy::too_many_lines)]
async fn interact_loop(
    client: &mut BmuxClient,
    run_id: Uuid,
    execution: process::ExecutionRef,
) -> Result<process::RunStatus> {
    let reference = pane::TerminalExecutionRef {
        session_id: execution.session_id,
        pane_id: execution.pane_id,
        execution_token: execution.execution_token,
    };
    let mut dimensions = viewport()?;
    let mut last_frame = None;
    let mut target_exit_observed: Option<std::time::Instant> = None;
    loop {
        let status = process::client::status(&mut *client, run_id)
            .await
            .context(InteractiveFailureKind::Transport)?
            .map_err(|error| anyhow::anyhow!("interactive authority unavailable: {error:?}"))?;
        ensure!(
            status.run_id == run_id
                && status
                    .execution
                    .as_ref()
                    .is_some_and(|target| target.session_id == execution.session_id
                        && target.pane_id == execution.pane_id
                        && target.execution_token == execution.execution_token),
            "interactive authority changed; input suspended"
        );
        ensure!(
            !status.recovery_required && status.errors.is_empty(),
            "interactive execution requires recovery: {:?}",
            status.errors
        );
        if let Some(observed) = target_exit_observed
            && !status.output_drained
            && observed.elapsed() >= POST_EXIT_DRAIN_GRACE
        {
            return Ok(status); // Restore terminal; capture stays pending, never finalized.
        }
        if status.target_exited {
            target_exit_observed.get_or_insert_with(std::time::Instant::now);
        }
        let current = viewport()?;
        if current != dimensions && !status.target_exited {
            recorded_resize(client, run_id, current).await?;
            dimensions = current;
            last_frame = None;
        }
        let screen = presentation::client::present(&mut *client, run_id)
            .await
            .context("interactive recording requires advertised bmux.recording.process.presentation.v1; refusing legacy screen fallback").context(InteractiveFailureKind::Transport)?
            .map_err(|error| anyhow::anyhow!("interactive presentation unavailable: {error:?}"))?;
        ensure!(
            screen.execution == reference,
            "interactive screen identity changed; input suspended"
        );
        ensure!(
            screen.cols == dimensions.0 && screen.rows == dimensions.1,
            "interactive viewport must be recorded before rendering; input suspended"
        );
        ensure!(
            screen.encoded_grid.len() <= MAX_SNAPSHOT,
            "interactive snapshot exceeds budget"
        );
        let revision = (
            screen.output_watermark,
            screen.cols,
            screen.rows,
            screen.encoded_grid.clone(),
        );
        let modes = if last_frame.as_ref() == Some(&revision) {
            let snapshot: GridSnapshot = serde_json::from_slice(&screen.encoded_grid)?;
            modes_from_protocol(snapshot.protocol)?
        } else {
            let modes = render(
                &mut std::io::stdout().lock(),
                &screen.encoded_grid,
                dimensions.0,
                dimensions.1,
            )?;
            last_frame = Some(revision);
            modes
        };
        if status.output_drained && status.terminal_session_ended {
            ensure!(
                screen.output_watermark >= status.output_watermark,
                "final interactive grid has not reached canonical output boundary"
            );
            return Ok(status);
        }
        if status.target_exited {
            tokio::time::sleep(FRAME_INTERVAL).await;
            continue;
        }
        // At most one input effect per validated authoritative snapshot. Queued
        // events remain in the bounded OS terminal queue rather than our heap.
        if let Some(event) = poll_input()? {
            if matches!(event, Event::Resize(_, _)) {
                // Read current ioctl geometry rather than applying an older
                // queued resize. Acknowledgment precedes the next presentation.
                let current = viewport()?;
                if current != dimensions {
                    recorded_resize(client, run_id, current).await?;
                    dimensions = current;
                    last_frame = None;
                }
                continue;
            }
            dispatch_input(client, run_id, event, modes).await?;
        }
        tokio::time::sleep(FRAME_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseButton};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn application_keys_are_not_bmux_hotkeys() {
        assert_eq!(
            encode_input(
                key(KeyCode::Char('a'), KeyModifiers::CONTROL),
                InputModes::default()
            )
            .unwrap(),
            Some(vec![1])
        );
        assert_eq!(
            encode_input(
                key(KeyCode::Char('c'), KeyModifiers::NONE),
                InputModes::default()
            )
            .unwrap(),
            Some(b"c".to_vec())
        );
        assert_eq!(
            encode_input(
                key(KeyCode::Char('f'), KeyModifiers::ALT),
                InputModes::default()
            )
            .unwrap(),
            Some(b"\x1bf".to_vec())
        );
        let modes = InputModes {
            application_cursor: true,
            ..InputModes::default()
        };
        assert_eq!(
            encode_input(key(KeyCode::Up, KeyModifiers::NONE), modes).unwrap(),
            Some(b"\x1bOA".to_vec())
        );
    }

    #[test]
    fn paste_modes_and_budget_are_explicit() {
        let modes = InputModes {
            bracketed_paste: true,
            ..InputModes::default()
        };
        assert_eq!(
            encode_input(Event::Paste("line\n".into()), modes).unwrap(),
            Some(b"\x1b[200~line\n\x1b[201~".to_vec())
        );
        assert!(encode_input(Event::Paste("x".repeat(MAX_INPUT)), modes).is_err());
        assert_eq!(
            encode_input(Event::Paste("plain".into()), InputModes::default()).unwrap(),
            Some(b"plain".to_vec())
        );
    }

    #[test]
    fn unsupported_job_control_does_not_send_or_terminate() {
        assert!(
            encode_input(
                key(KeyCode::Char('z'), KeyModifiers::CONTROL),
                InputModes::default()
            )
            .unwrap_err()
            .to_string()
            .contains("suspension is unsupported")
        );
        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(
            encode_input(Event::Key(release), InputModes::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mouse_routes_only_requested_modes() {
        let mouse = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 4,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let modes = InputModes {
            mouse_press_release: true,
            mouse_sgr: true,
            ..InputModes::default()
        };
        assert_eq!(
            encode_mouse(mouse, modes).unwrap(),
            Some(b"\x1b[<0;5;4m".to_vec())
        );
        assert!(
            encode_mouse(mouse, InputModes::default())
                .unwrap()
                .is_none()
        );
        let x10 = InputModes {
            mouse_x10: true,
            ..InputModes::default()
        };
        assert!(encode_mouse(mouse, x10).unwrap().is_none());
        let legacy = InputModes {
            mouse_press_release: true,
            ..InputModes::default()
        };
        assert!(
            encode_mouse(
                MouseEvent {
                    column: 300,
                    ..mouse
                },
                legacy
            )
            .is_err()
        );
    }

    #[test]
    fn grid_repaint_never_forwards_application_queries() {
        let mut stream = TerminalGridStream::new(8, 2, GridLimits::default()).unwrap();
        stream.process(b"hello\x1b[6n\x1b[c\x1b[?1h\x1b[?2004h");
        let encoded = serde_json::to_vec(&stream.snapshot(0, 2)).unwrap();
        let mut frame = Vec::new();
        let modes = render(&mut frame, &encoded, 8, 2).unwrap();
        assert!(modes.application_cursor && modes.bracketed_paste);
        assert!(frame.windows(5).any(|part| part == b"hello"));
        assert!(!frame.windows(4).any(|part| part == b"\x1b[6n"));
        assert!(!frame.windows(3).any(|part| part == b"\x1b[c"));
        assert!(render(&mut frame, &encoded, 9, 2).is_err());
        assert!(render(&mut frame, b"broken", 8, 2).is_err());
    }

    #[test]
    fn wide_graphemes_keep_display_columns() {
        let mut source = TerminalGridStream::new(8, 2, GridLimits::default()).unwrap();
        source.process("界A🙂B".as_bytes());
        let bytes = repaint(source.grid()).unwrap();
        let mut displayed = TerminalGridStream::new(8, 2, GridLimits::default()).unwrap();
        displayed.process(&bytes);
        assert_eq!(
            bmux_terminal_grid::visible_text(source.grid(), 0, 2),
            bmux_terminal_grid::visible_text(displayed.grid(), 0, 2)
        );
    }

    #[test]
    fn failure_classification_preserves_chain_and_restoration_evidence() {
        let error = combine_restore::<()>(
            Err(anyhow::anyhow!("socket gone").context(InteractiveFailureKind::Transport)),
            Ok(()),
        )
        .unwrap_err();
        assert_eq!(failure_kind(&error), InteractiveFailureKind::Transport);
        assert_eq!(terminal_restored(&error), Some(true));
        assert!(format!("{error:#}").contains("socket gone"));
        let error = combine_restore::<()>(
            Err(anyhow::anyhow!("hangup").context(InteractiveFailureKind::Disconnect)),
            Err(anyhow::anyhow!("restore rejected")),
        )
        .unwrap_err();
        assert_eq!(failure_kind(&error), InteractiveFailureKind::Restoration);
        assert_eq!(terminal_restored(&error), Some(false));
        let unknown = anyhow::anyhow!("pre-entry failure");
        assert_eq!(terminal_restored(&unknown), None);
        assert_eq!(failure_kind(&unknown), InteractiveFailureKind::Operation);
    }

    #[test]
    fn failed_render_and_restoration_remain_errors() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("display lost"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let stream = TerminalGridStream::new(8, 2, GridLimits::default()).unwrap();
        let encoded = serde_json::to_vec(&stream.snapshot(0, 2)).unwrap();
        assert!(render(&mut FailingWriter, &encoded, 8, 2).is_err());
        let error = combine_restore::<()>(
            Err(anyhow::anyhow!("provider lost")),
            Err(anyhow::anyhow!("restore lost")),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("provider lost"));
        assert!(format!("{error:#}").contains("restore lost"));
        assert!(combine_restore(Ok(()), Err(anyhow::anyhow!("restore lost"))).is_err());
    }
}
