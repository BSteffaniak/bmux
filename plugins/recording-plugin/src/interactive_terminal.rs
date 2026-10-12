//! Undecorated recorded-execution frontend. The server owns parsing and query replies.
use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bmux_attach_pipeline::scene_pipeline::UndecoratedGridPresenter;
use bmux_attach_pipeline::terminal::{TerminalIo, TerminalOptions};
use bmux_client::BmuxClient;
use bmux_keyboard::encode::{KeyEncodingModes, encode_key_with_modes};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4 as pane;
use bmux_recording_plugin_api::{
    recording_process_presentation_v1 as presentation,
    recording_process_presentation_wait_v1 as presentation_wait, recording_process_v4 as process,
};
use bmux_terminal_grid::{GridLimits, GridSnapshot, TerminalGridStream};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use uuid::Uuid;

const MAX_INPUT: usize = 16_384;
const MAX_SNAPSHOT_BYTES: u32 = 4 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 4 * 1024 * 1024;
// Give buffered output a bounded presentation grace without waiting for
// surviving slave holders; capture remains pending until real EOF.
const POST_EXIT_DRAIN_GRACE: Duration = Duration::from_millis(250);

type InputModes = bmux_terminal_grid::ProtocolState;

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
    root: &std::path::Path,
    client: &mut BmuxClient,
    run_id: Uuid,
    execution: process::ExecutionRef,
) -> Result<process::RunStatus> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut suspend =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP))?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive recording requires terminal stdin and stdout"
    );
    let mut receiving =
        crate::process_bootstrap::connect_owned(root, "recording-presentation").await?;
    let observing = crate::process_bootstrap::connect_owned(root, "recording-status").await?;
    let caps = presentation_wait::client::capabilities(&mut receiving).await
        .context("interactive recording requires bmux.recording.process.presentation.wait.v1; no legacy polling fallback")?;
    ensure!(
        caps.schema_version == 1
            && caps.token_bytes == 32
            && caps.max_timeout_ms >= 1000
            && caps.max_response_bytes >= MAX_SNAPSHOT_BYTES,
        "unsupported presentation wait capabilities"
    );
    let mut guard = TerminalIo::new(std::io::stdout());
    if let Err(error) = guard.enter(TerminalOptions {
        keyboard_enhanced: false,
        mouse_capture: true,
        bracketed_paste: true,
        focus_reporting: false,
        alternate_screen: true,
        disable_wrap: true,
    }) {
        return combine_restore(Err(error.into()), guard.restore().map_err(Into::into));
    }
    let operation = tokio::select! {
        result = interact_loop(client, receiving, observing, &mut guard, run_id, execution) => result,
        _ = interrupt.recv() => Err(anyhow::anyhow!("interactive frontend interrupted; execution retained, pending mutations must be reconciled").context(InteractiveFailureKind::Interrupted)),
        _ = terminate.recv() => Err(anyhow::anyhow!("interactive frontend terminated; execution retained, pending mutations must be reconciled").context(InteractiveFailureKind::Interrupted)),
        _ = suspend.recv() => Err(anyhow::anyhow!("outer terminal suspension unsupported; execution retained for recovery").context(InteractiveFailureKind::Unsupported)),
        _ = hangup.recv() => Err(anyhow::anyhow!("interactive terminal disconnected; execution retained for recovery").context(InteractiveFailureKind::Disconnect)),
    };
    combine_restore(operation, guard.restore().map_err(Into::into))
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

async fn send_batch(
    client: &mut BmuxClient,
    run_id: Uuid,
    reference: &pane::TerminalExecutionRef,
    data: Vec<u8>,
) -> Result<()> {
    let mutation_id = Uuid::new_v4();
    let bytes = data.len();
    let receipt = process::client::send(&mut *client, run_id, data, mutation_id)
        .await
        .with_context(|| {
            format!("interactive input outcome uncertain; reconcile mutation_id={mutation_id}")
        })
        .context(InteractiveFailureKind::Transport)?
        .map_err(|error| {
            anyhow::anyhow!(
                "interactive input rejected; mutation_id={mutation_id}; input suspended: {error:?}"
            )
        })?;
    validate_input_receipt(&receipt, reference, mutation_id, bytes)
}

fn validate_input_receipt(
    receipt: &pane::TerminalInputReceipt,
    reference: &pane::TerminalExecutionRef,
    mutation_id: Uuid,
    bytes: usize,
) -> Result<()> {
    ensure!(
        receipt.execution == *reference && receipt.input_mutation_id == mutation_id,
        "input receipt identity mismatch; reconcile mutation_id={mutation_id}"
    );
    ensure!(
        matches!(receipt.phase, pane::TerminalInputPhase::Completed)
            && receipt.error.is_none()
            && receipt.admitted_bytes == u64::try_from(bytes)?
            && receipt.acknowledged_bytes == receipt.admitted_bytes,
        "input outcome unresolved; reconcile mutation_id={mutation_id}; receipt={receipt:?}"
    );
    Ok(())
}

const INPUT_QUEUE_ITEMS: usize = 128;
const INPUT_QUEUE_BYTES: usize = 64 * 1024;

enum ControlCommand {
    Input(Vec<u8>),
    Resize((u16, u16)),
}

#[derive(Default)]
struct ControlQueue {
    commands: std::collections::VecDeque<ControlCommand>,
    bytes: usize,
}

impl ControlQueue {
    fn push(&mut self, command: ControlCommand) -> Result<()> {
        let bytes = match &command {
            ControlCommand::Input(data) => data.len(),
            ControlCommand::Resize(_) => 0,
        };
        ensure!(
            bytes <= MAX_INPUT
                && self.commands.len() < INPUT_QUEUE_ITEMS
                && self.bytes.saturating_add(bytes) <= INPUT_QUEUE_BYTES,
            "interactive control backpressure: bounded queue full; pending effects require reconciliation"
        );
        self.bytes += bytes;
        self.commands.push_back(command);
        Ok(())
    }

    fn take_batch(&mut self) -> Option<(ControlCommand, usize)> {
        let command = self.commands.pop_front()?;
        let ControlCommand::Input(mut data) = command else {
            return Some((command, 1));
        };
        self.bytes -= data.len();
        let mut items = 1;
        while let Some(ControlCommand::Input(next)) = self.commands.front() {
            if data.len().saturating_add(next.len()) > MAX_INPUT {
                break;
            }
            if let Some(ControlCommand::Input(next)) = self.commands.pop_front() {
                self.bytes -= next.len();
                data.extend_from_slice(&next);
                items += 1;
            }
        }
        Some((ControlCommand::Input(data), items))
    }
}

struct ControlCompletion {
    items: usize,
    resized: Option<(u16, u16)>,
}

type SharedControlQueue = std::sync::Arc<std::sync::Mutex<ControlQueue>>;

fn queue_control(
    queue: &SharedControlQueue,
    wake: &tokio::sync::Notify,
    command: ControlCommand,
) -> Result<()> {
    queue
        .lock()
        .map_err(|_| anyhow::anyhow!("control queue poisoned"))?
        .push(command)?;
    wake.notify_one();
    Ok(())
}

async fn drive_controls(
    client: &mut BmuxClient,
    run_id: Uuid,
    reference: &pane::TerminalExecutionRef,
    queue: &SharedControlQueue,
    wake: &tokio::sync::Notify,
    completed: tokio::sync::mpsc::Sender<ControlCompletion>,
) -> Result<()> {
    loop {
        let command = queue
            .lock()
            .map_err(|_| anyhow::anyhow!("control queue poisoned"))?
            .take_batch();
        let Some((command, items)) = command else {
            wake.notified().await;
            continue;
        };
        // No timer or collection delay: first available input is sent now.
        // Subsequent queued bytes share one durable mutation, never a resize.
        let resized = match command {
            ControlCommand::Input(data) => {
                send_batch(client, run_id, reference, data).await?;
                None
            }
            ControlCommand::Resize(dimensions) => {
                recorded_resize(client, run_id, dimensions).await?;
                Some(dimensions)
            }
        };
        completed
            .send(ControlCompletion { items, resized })
            .await
            .map_err(|_| {
                anyhow::anyhow!("control completion receiver disconnected; outcome retained")
            })?;
    }
}

struct ReceivingTasks(Vec<tokio::task::JoinHandle<()>>);
impl Drop for ReceivingTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

fn validate_screen(
    screen: &pane::TerminalExecutionScreen,
    reference: &pane::TerminalExecutionRef,
) -> Result<GridSnapshot> {
    ensure!(
        &screen.execution == reference,
        "interactive screen identity changed; input suspended"
    );
    ensure!(
        screen.encoded_grid.len() <= MAX_SNAPSHOT,
        "interactive snapshot exceeds budget"
    );
    let snapshot: GridSnapshot = serde_json::from_slice(&screen.encoded_grid)?;
    ensure!(
        snapshot.width == screen.cols && snapshot.height == screen.rows,
        "incoherent presentation geometry"
    );
    ensure!(
        snapshot
            .rows
            .iter()
            .flat_map(|row| &row.runs)
            .all(|run| !run.text.chars().any(char::is_control)),
        "interactive grid contains control bytes instead of display text"
    );
    TerminalGridStream::from_snapshot(&snapshot, GridLimits::default())
        .context("validating authoritative interactive grid")?;
    modes_from_protocol(snapshot.protocol)?;
    Ok(snapshot)
}

fn validate_status(
    status: &process::RunStatus,
    run_id: Uuid,
    reference: &pane::TerminalExecutionRef,
) -> Result<()> {
    ensure!(
        status.run_id == run_id
            && status
                .execution
                .as_ref()
                .is_some_and(|target| target.session_id == reference.session_id
                    && target.pane_id == reference.pane_id
                    && target.execution_token == reference.execution_token),
        "interactive execution identity changed; input suspended"
    );
    ensure!(
        !status.recovery_required && status.errors.is_empty(),
        "interactive execution requires recovery: {:?}",
        status.errors
    );
    Ok(())
}

async fn wait_for_exit_grace(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

struct ReceivingChannels {
    tasks: ReceivingTasks,
    frames: tokio::sync::mpsc::Receiver<
        Result<bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::PresentationFrame>,
    >,
    statuses: tokio::sync::mpsc::Receiver<Result<process::RunStatus>>,
}

fn spawn_receiving(
    mut receiving: BmuxClient,
    mut observing: BmuxClient,
    run_id: Uuid,
) -> ReceivingChannels {
    // Separate authenticated transports: neither receiving nor durable status
    // reconciliation can head-of-line block the ordered control connection.
    // One pending complete frame and one status are the entire retained queue.
    let (frames_tx, frames) = tokio::sync::mpsc::channel(1);
    let (status_tx, statuses) = tokio::sync::mpsc::channel(1);
    let frame_task = tokio::spawn(async move {
        let mut after = None;
        loop {
            let result = presentation_wait::client::wait_present(
                &mut receiving,
                run_id,
                after.clone(),
                1000,
                MAX_SNAPSHOT_BYTES,
            )
            .await
            .context("presentation receiving transport failed")
            .and_then(|value| {
                value.map_err(|error| anyhow::anyhow!("presentation wait rejected: {error:?}"))
            });
            match result {
                Ok(Some(frame)) => {
                    after = Some(frame.revision.clone());
                    if frames_tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = frames_tx.send(Err(error)).await;
                    break;
                }
            }
        }
    });
    let status_task = tokio::spawn(async move {
        loop {
            let result = process::client::status(&mut observing, run_id)
                .await
                .context("independent status transport failed")
                .and_then(|value| {
                    value.map_err(|error| {
                        anyhow::anyhow!("interactive authority unavailable: {error:?}")
                    })
                });
            let failed = result.is_err();
            if status_tx.send(result).await.is_err() || failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    ReceivingChannels {
        tasks: ReceivingTasks(vec![frame_task, status_task]),
        frames,
        statuses,
    }
}

fn present_frame(
    frame: &bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::PresentationFrame,
    reference: &pane::TerminalExecutionRef,
    dimensions: (u16, u16),
    terminal: &mut TerminalIo<std::io::Stdout>,
    presenter: &mut UndecoratedGridPresenter,
) -> Result<Option<(u64, InputModes)>> {
    ensure!(
        frame.revision.len() == 32,
        "invalid presentation revision token"
    );
    let snapshot = validate_screen(&frame.screen, reference)?;
    if (frame.screen.cols, frame.screen.rows) != dimensions {
        return Ok(None);
    }
    presenter.present(terminal, &snapshot)?;
    Ok(Some((
        frame.screen.output_watermark,
        modes_from_protocol(snapshot.protocol)?,
    )))
}

// Keep completion, resize barriers and input scheduling in one select state
// machine: splitting its arms would obscure the pending-effect safety boundary.
#[allow(clippy::too_many_lines)]
async fn interact_loop(
    client: &mut BmuxClient,
    receiving: BmuxClient,
    observing: BmuxClient,
    terminal: &mut TerminalIo<std::io::Stdout>,
    run_id: Uuid,
    execution: process::ExecutionRef,
) -> Result<process::RunStatus> {
    let reference = pane::TerminalExecutionRef {
        session_id: execution.session_id,
        pane_id: execution.pane_id,
        execution_token: execution.execution_token,
    };
    let ReceivingChannels {
        tasks: _tasks,
        mut frames,
        mut statuses,
    } = spawn_receiving(receiving, observing, run_id);
    let mut dimensions = terminal.geometry()?;
    recorded_resize(client, run_id, dimensions).await?;
    let mut presenter = UndecoratedGridPresenter::new(dimensions.0, dimensions.1);
    let mut modes = None;
    let mut observed = None;
    let mut displayed_watermark = None;
    let queue = std::sync::Arc::new(std::sync::Mutex::new(ControlQueue::default()));
    let wake = tokio::sync::Notify::new();
    let (completed_tx, mut completed) = tokio::sync::mpsc::channel(1);
    let controls = drive_controls(client, run_id, &reference, &queue, &wake, completed_tx);
    tokio::pin!(controls);
    let mut pending = 0_usize;
    let mut resize_pending = false;
    let mut retained_frame = None;
    let mut target_exit_deadline = None;
    loop {
        if !resize_pending && let Some(frame) = retained_frame.take() {
            let frame: bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::PresentationFrame = frame;
            if let Some((watermark, protocol)) =
                present_frame(&frame, &reference, dimensions, terminal, &mut presenter)?
            {
                displayed_watermark = Some(watermark);
                modes = Some(protocol);
            }
        }
        if let (Some(status), Some(watermark)) = (&observed, displayed_watermark) {
            let status: &process::RunStatus = status;
            if status.output_drained
                && status.terminal_session_ended
                && watermark >= status.output_watermark
                && modes.is_some()
                && pending == 0
                && !resize_pending
            {
                return Ok(status.clone());
            }
        }
        tokio::select! {
            // Unbiased selection keeps continuous output from starving input.
            result = &mut controls => {
                return Err(result.err().unwrap_or_else(|| anyhow::anyhow!("control worker stopped"))
                    .context(format!("{pending} queued/in-flight input effects unresolved; execution retained; no retry")));
            }
            outcome = completed.recv() => {
                let outcome = outcome.context("control worker disconnected with pending effects")?;
                pending = pending.checked_sub(outcome.items).context("control completion accounting mismatch")?;
                if let Some(current) = outcome.resized {
                    dimensions = current;
                    resize_pending = false;
                }
            }
            frame = frames.recv() => {
                retained_frame = Some(frame.context("presentation receiver ended")??);
            }
            () = wait_for_exit_grace(target_exit_deadline) => {
                ensure!(pending == 0,
                    "target exited with {pending} queued/in-flight control effects unresolved; execution retained; no retry");
                return observed.context("target exit status unavailable");
            }
            status = statuses.recv() => {
                let status = status.context("status receiver ended")??;
                validate_status(&status, run_id, &reference)?;
                if status.target_exited && target_exit_deadline.is_none() {
                    target_exit_deadline = Some(tokio::time::Instant::now() + POST_EXIT_DRAIN_GRACE);
                }
                observed = Some(status);
            }
            event = terminal.next_event(), if modes.is_some() && !observed.as_ref().is_some_and(|status| status.target_exited || status.terminal_session_ended) => {
                let event = event.context(InteractiveFailureKind::Disconnect)?.context("interactive terminal disconnected; execution retained")?;
                if matches!(event, Event::Resize(_, _)) {
                    let current = terminal.geometry()?;
                    if current != dimensions {
                        queue_control(&queue, &wake, ControlCommand::Resize(current))?;
                        pending += 1;
                        resize_pending = true;
                        modes = None;
                    }
                } else if let Some(data) = encode_input(event, modes.context("input modes unavailable")?)? {
                    queue_control(&queue, &wake, ControlCommand::Input(data))?;
                    pending += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseButton};
    use std::io::Write;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn queued_input_groups_only_adjacent_bytes_before_resize() {
        let mut queue = ControlQueue::default();
        queue.push(ControlCommand::Input(b"a".to_vec())).unwrap();
        let Some((ControlCommand::Input(first), items)) = queue.take_batch() else {
            panic!("first input missing");
        };
        assert_eq!((first, items), (b"a".to_vec(), 1));
        queue.push(ControlCommand::Input(b"b".to_vec())).unwrap();
        queue.push(ControlCommand::Input(b"c".to_vec())).unwrap();
        queue.push(ControlCommand::Resize((93, 31))).unwrap();
        queue.push(ControlCommand::Input(b"d".to_vec())).unwrap();
        let Some((ControlCommand::Input(group), items)) = queue.take_batch() else {
            panic!("group missing");
        };
        assert_eq!((group, items), (b"bc".to_vec(), 2));
        assert!(matches!(
            queue.take_batch(),
            Some((ControlCommand::Resize((93, 31)), 1))
        ));
        let Some((ControlCommand::Input(last), items)) = queue.take_batch() else {
            panic!("last input missing");
        };
        assert_eq!((last, items), (b"d".to_vec(), 1));
        assert_eq!(queue.bytes, 0);
        assert!(queue.take_batch().is_none());
    }

    #[test]
    fn control_queue_enforces_item_byte_and_batch_bounds() {
        let mut queue = ControlQueue::default();
        for _ in 0..INPUT_QUEUE_ITEMS {
            queue.push(ControlCommand::Input(vec![1])).unwrap();
        }
        assert!(queue.push(ControlCommand::Resize((80, 24))).is_err());
        let mut queue = ControlQueue::default();
        for _ in 0..4 {
            queue
                .push(ControlCommand::Input(vec![1; MAX_INPUT]))
                .unwrap();
        }
        assert_eq!(queue.bytes, INPUT_QUEUE_BYTES);
        assert!(queue.push(ControlCommand::Input(vec![2])).is_err());
        let Some((ControlCommand::Input(data), items)) = queue.take_batch() else {
            panic!("batch missing");
        };
        assert_eq!(data.len(), MAX_INPUT);
        assert_eq!(items, 1);
        assert_eq!(queue.bytes, INPUT_QUEUE_BYTES - MAX_INPUT);
        assert!(
            queue
                .push(ControlCommand::Input(vec![2; MAX_INPUT + 1]))
                .is_err()
        );
    }

    #[test]
    fn unresolved_or_wrong_batch_receipt_never_counts_as_completion() {
        let reference = pane::TerminalExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        };
        let mutation = Uuid::new_v4();
        let mut receipt = pane::TerminalInputReceipt {
            execution: reference.clone(),
            input_mutation_id: mutation,
            revision: 2,
            phase: pane::TerminalInputPhase::Completed,
            admitted_bytes: 64,
            acknowledged_bytes: 64,
            error: None,
        };
        validate_input_receipt(&receipt, &reference, mutation, 64).unwrap();
        for phase in [
            pane::TerminalInputPhase::Accepted,
            pane::TerminalInputPhase::Writing,
            pane::TerminalInputPhase::Uncertain,
            pane::TerminalInputPhase::Rejected,
        ] {
            receipt.phase = phase;
            assert!(validate_input_receipt(&receipt, &reference, mutation, 64).is_err());
        }
        receipt.phase = pane::TerminalInputPhase::Completed;
        assert!(validate_input_receipt(&receipt, &reference, Uuid::new_v4(), 64).is_err());
        receipt.execution.execution_token = Uuid::new_v4();
        assert!(validate_input_receipt(&receipt, &reference, mutation, 64).is_err());
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
        let snapshot = stream.snapshot(0, 2);
        let modes = modes_from_protocol(snapshot.protocol).unwrap();
        UndecoratedGridPresenter::new(8, 2)
            .present(&mut frame, &snapshot)
            .unwrap();
        assert!(modes.application_cursor && modes.bracketed_paste);
        assert!(frame.windows(5).any(|part| part == b"hello"));
        assert!(!frame.windows(4).any(|part| part == b"\x1b[6n"));
        assert!(!frame.windows(3).any(|part| part == b"\x1b[c"));
        let reference = pane::TerminalExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        };
        let mut screen = pane::TerminalExecutionScreen {
            execution: reference.clone(),
            output_watermark: 0,
            cols: 8,
            rows: 2,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            text: String::new(),
            encoded_grid: encoded,
        };
        validate_screen(&screen, &reference).unwrap();
        screen.cols = 9;
        assert!(validate_screen(&screen, &reference).is_err());
        screen.cols = 8;
        screen.encoded_grid = b"broken".to_vec();
        assert!(validate_screen(&screen, &reference).is_err());
    }

    #[test]
    fn wide_graphemes_keep_display_columns() {
        let mut source = TerminalGridStream::new(8, 2, GridLimits::default()).unwrap();
        source.process("界A🙂B".as_bytes());
        let mut bytes = Vec::new();
        UndecoratedGridPresenter::new(8, 2)
            .present(&mut bytes, &source.snapshot(0, 2))
            .unwrap();
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
        assert!(
            UndecoratedGridPresenter::new(8, 2)
                .present(&mut FailingWriter, &stream.snapshot(0, 2))
                .is_err()
        );
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
