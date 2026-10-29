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
    recording_process_live_presentation_v1 as live,
    recording_process_presentation_v1 as presentation, recording_process_v4 as process,
};
use bmux_terminal_grid::{GridLimits, GridSnapshot, TerminalGridStream};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use uuid::Uuid;

const MAX_INPUT: usize = 16_384;
const MAX_SNAPSHOT_BYTES: u32 = 4 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 4 * 1024 * 1024;
// Give buffered output a bounded presentation grace without waiting for
// surviving slave holders; capture remains pending until real EOF.
// This is a presentation budget, not the authoritative EOF/cleanup deadline.
const POST_EXIT_DRAIN_GRACE: Duration = Duration::from_millis(250);
// Match the existing execution wait contract's maximum bounded wait.
const POST_EXIT_CLEANUP_TIMEOUT_MS: u32 = 30_000;

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
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('z' | 'Z'))
            {
                return Err(
                    anyhow::anyhow!(InteractiveFailureKind::Unsupported).context(
                        "outer terminal suspension is unsupported; execution retained for recovery",
                    ),
                );
            }
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
    let mut stop_events = crate::platform::StopEvents::new()?;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive recording requires terminal stdin and stdout"
    );
    let mut receiving =
        crate::process_bootstrap::connect_owned(root, "recording-presentation").await?;
    let observing = crate::process_bootstrap::connect_owned(root, "recording-status").await?;
    let caps = live::client::capabilities(&mut receiving).await
        .context("interactive recording requires bmux.recording.process.live.presentation.v1; no legacy fallback")?;
    ensure!(
        caps.schema_version == 1
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
    let stop = stop_events.recv();
    // Signals terminate the loop cooperatively: dropping its future would skip
    // asynchronous stream close and could strand a live epoch on every repeat.
    let operation = interact_loop(
        client, receiving, observing, &mut guard, run_id, execution, stop,
    )
    .await;
    combine_restore(operation, guard.restore().map_err(Into::into))?;
    // Presentation may stop before retirement/EOF. Wait only after restoring
    // the outer terminal, through the owner's bounded, event-driven contract.
    wait_for_post_exit_cleanup(client, run_id, POST_EXIT_CLEANUP_TIMEOUT_MS)
        .await
        .map_err(|error| error.context(RestorationObserved(true)))
}

fn post_exit_cleanup_complete(status: &process::RunStatus) -> bool {
    status.target_exited
        && status.terminal_session_ended
        && status.capture_eof
        && status.output_drained
        && status.cleanup_complete
        && !status.recovery_required
        && status.errors.is_empty()
}

async fn wait_for_post_exit_cleanup(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    run_id: Uuid,
    timeout_ms: u32,
) -> Result<process::RunStatus> {
    let current = process::client::status(client, run_id)
        .await?
        .map_err(|error| anyhow::anyhow!("recording status: {error:?}"))?;
    if !post_exit_cleanup_complete(&current) {
        process::client::wait(client, run_id, timeout_ms)
            .await?
            .map_err(|error| anyhow::anyhow!("recording cleanup wait: {error:?}"))?;
    }
    // Reconciliation and execution status can straddle EOF. Do not finish from
    // the pre-wait snapshot (or infer EOF from an exit/retirement receipt).
    let current = process::client::status(client, run_id)
        .await?
        .map_err(|error| anyhow::anyhow!("recording status: {error:?}"))?;
    ensure!(
        post_exit_cleanup_complete(&current),
        "terminal restored; capture/cleanup pending after bounded owner wait; run {} retained at {}; real EOF, drain and cleanup are required before finish",
        run_id,
        current.runtime_root
    );
    Ok(current)
}

async fn recorded_resize(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
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

#[cfg(test)]
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
    closed: bool,
}

impl ControlQueue {
    fn shutdown(&mut self) {
        self.closed = true;
        self.commands.clear();
        self.bytes = 0;
    }

    fn push(&mut self, command: ControlCommand) -> Result<()> {
        ensure!(
            !self.closed,
            "interactive control queue closed; input not resumed"
        );
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
        if self.closed {
            return None;
        }
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
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    run_id: Uuid,
    reference: &pane::TerminalExecutionRef,
    queue: &SharedControlQueue,
    wake: &tokio::sync::Notify,
    completed: tokio::sync::mpsc::Sender<ControlCompletion>,
) -> Result<()> {
    use bmux_recording_plugin_api::recording_process_human_input_v1::client as human;
    let capabilities = human::capabilities(client).await?;
    ensure!(
        capabilities.protocol_version == 1 && capabilities.max_payload_bytes >= 16_384,
        "unsupported human-input contract; refusing durable fallback"
    );
    let stream = tokio::time::timeout(Duration::from_secs(2), human::open(client, run_id))
        .await
        .context("human stream open timed out; outcome unknown; no automatic reopen")?
        .context("human stream open outcome unknown; no automatic reopen")?
        .map_err(|error| anyhow::anyhow!("human stream open: {error:?}"))?;
    // All paths after open attempt close, including identity/ack failures. The
    // owner retains an idle expiry for a lost open response or broken transport.
    let result: Result<()> = async {
        ensure!(
            &stream.execution == reference,
            "human stream execution mismatch"
        );
        let mut sequence = 1_u64;
        loop {
            let command = {
                let mut queue = queue
                    .lock()
                    .map_err(|_| anyhow::anyhow!("control queue poisoned"))?;
                if queue.closed {
                    break;
                }
                queue.take_batch()
            };
            let Some((command, items)) = command else {
                wake.notified().await;
                continue;
            };
            // Only already queued bytes coalesce; no timer, durable mutation,
            // automatic resend, or reconnection with outstanding input.
            let resized = match command {
                ControlCommand::Input(data) => {
                    let expected_bytes = u64::try_from(data.len())?;
                    let ack = tokio::time::timeout(
                        Duration::from_secs(6),
                        human::push(client, run_id, stream.clone(), sequence, data),
                    )
                    .await
                    .context("human input timeout; outcome unknown; no retry")?
                    .context("human input transport outcome unknown; no retry")?
                    .map_err(|error| anyhow::anyhow!("human input: {error:?}; no retry"))?;
                    validate_human_ack(&ack, &stream, sequence, expected_bytes)?;
                    sequence = sequence
                        .checked_add(1)
                        .context("human sequence exhausted")?;
                    None
                }
                ControlCommand::Resize(dimensions) => {
                    tokio::time::timeout(
                        Duration::from_secs(6),
                        recorded_resize(client, run_id, dimensions),
                    )
                    .await
                    .context("resize timed out; outcome unknown; no retry")??;
                    Some(dimensions)
                }
            };
            // Teardown closes the receiver to unblock a worker finishing its
            // current operation. Never drain queued effects during teardown.
            if completed
                .send(ControlCompletion { items, resized })
                .await
                .is_err()
            {
                break;
            }
        }
        Ok(())
    }
    .await;
    let close = tokio::time::timeout(Duration::from_secs(2), human::close(client, run_id, stream))
        .await
        .context("human stream close timed out; epoch may remain until idle expiry")
        .and_then(|value| value.map_err(anyhow::Error::from))
        .and_then(|value| value.map_err(|error| anyhow::anyhow!("human stream close: {error:?}")));
    match (result, close) {
        (Ok(()), result) => result,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(close)) => {
            Err(error.context(format!("stream cleanup also failed: {close:#}")))
        }
    }
}

fn validate_human_ack(
    ack: &bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputAck,
    stream: &bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputStream,
    sequence: u64,
    expected_bytes: u64,
) -> Result<()> {
    ensure!(
        ack.stream == *stream && ack.sequence == sequence && ack.written_bytes == expected_bytes,
        "invalid human input acknowledgment; outcome unknown; no retry"
    );
    Ok(())
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
        Result<bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::LiveFrame>,
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
            let result = live::client::wait_present(
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
                    after = Some(frame.cursor.clone());
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

/// Received state is distinct from the presenter's successfully committed scene.
/// Watermark-only frames retain the last canonical snapshot without repainting.
#[derive(Default)]
struct LiveScene {
    cursor: Option<bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::LiveCursor>,
    snapshot: Option<GridSnapshot>,
    displayed: Option<(u64, u64, u64)>,
}

impl LiveScene {
    fn accept(
        &mut self,
        frame: bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::LiveFrame,
        reference: &pane::TerminalExecutionRef,
    ) -> Result<()> {
        let cursor = &frame.cursor;
        ensure!(
            cursor.execution == *reference && cursor.source_epoch == reference.execution_token,
            "live presentation source changed; input suspended; no automatic resume"
        );
        ensure!(
            !cursor.recovery_required,
            "live presentation requires recovery; input suspended"
        );
        let changed = if let Some(previous) = &self.cursor {
            ensure!(
                cursor.execution == previous.execution
                    && cursor.source_epoch == previous.source_epoch,
                "live source epoch changed; no automatic resume"
            );
            ensure!(
                cursor.source_reset_revision >= previous.source_reset_revision
                    && (cursor.source_reset_revision != previous.source_reset_revision
                        || (cursor.grid_revision >= previous.grid_revision
                            && cursor.geometry_revision >= previous.geometry_revision))
                    && cursor.projected_raw_watermark >= previous.projected_raw_watermark
                    && cursor.durable_raw_watermark >= previous.durable_raw_watermark
                    && (!previous.target_exited || cursor.target_exited)
                    && (!previous.output_drained || cursor.output_drained)
                    && (!previous.terminal_session_ended || cursor.terminal_session_ended),
                "live cursor regressed; input suspended"
            );
            cursor.source_reset_revision != previous.source_reset_revision
                || cursor.grid_revision != previous.grid_revision
                || cursor.geometry_revision != previous.geometry_revision
        } else {
            true
        };
        ensure!(
            !changed || frame.screen.is_some(),
            "changed live scene omitted its canonical screen"
        );
        let snapshot = if let Some(screen) = &frame.screen {
            ensure!(
                screen.output_watermark == cursor.projected_raw_watermark,
                "live screen projected watermark mismatch"
            );
            let snapshot = validate_screen(screen, reference)?;
            ensure!(
                snapshot.revision == cursor.grid_revision,
                "live canonical grid revision mismatch"
            );
            if let (Some(previous), Some(retained)) = (&self.cursor, &self.snapshot)
                && cursor.source_reset_revision == previous.source_reset_revision
                && cursor.geometry_revision == previous.geometry_revision
            {
                ensure!(
                    snapshot.width == retained.width && snapshot.height == retained.height,
                    "live geometry changed without geometry revision"
                );
            }
            Some(snapshot)
        } else {
            None
        };
        // Commit receive state only after the entire envelope validates. It is
        // not displayed authority until present() successfully flushes output.
        if let Some(snapshot) = snapshot {
            self.snapshot = Some(snapshot);
        }
        self.cursor = Some(frame.cursor);
        Ok(())
    }

    fn present(
        &mut self,
        dimensions: (u16, u16),
        terminal: &mut impl std::io::Write,
        presenter: &mut UndecoratedGridPresenter,
    ) -> Result<Option<(u64, InputModes)>> {
        let (Some(cursor), Some(snapshot)) = (&self.cursor, &self.snapshot) else {
            return Ok(None);
        };
        if (snapshot.width, snapshot.height) != dimensions {
            return Ok(None);
        }
        let revision = (
            cursor.source_reset_revision,
            cursor.grid_revision,
            cursor.geometry_revision,
        );
        if self.displayed != Some(revision) {
            if self
                .displayed
                .is_some_and(|(reset, _, _)| reset != cursor.source_reset_revision)
            {
                // A parser reset invalidates retained damage/cursor baselines.
                // Publish the replacement presenter only after its full repair
                // successfully reaches the outer terminal.
                let mut repair = UndecoratedGridPresenter::new(dimensions.0, dimensions.1);
                repair.present(terminal, snapshot)?;
                *presenter = repair;
            } else {
                presenter.present(terminal, snapshot)?;
            }
            self.displayed = Some(revision);
        }
        Ok(Some((
            cursor.projected_raw_watermark,
            modes_from_protocol(snapshot.protocol)?,
        )))
    }

    fn input_suspended(&self) -> bool {
        self.cursor.as_ref().is_some_and(|cursor| {
            cursor.target_exited
                || cursor.terminal_session_ended
                || cursor.recovery_required
                || self
                    .displayed
                    .is_none_or(|(reset, _, _)| reset != cursor.source_reset_revision)
        })
    }
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
    stop: impl std::future::Future<Output = anyhow::Error>,
) -> Result<process::RunStatus> {
    tokio::pin!(stop);
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
    let mut live_scene = LiveScene::default();
    let mut target_exit_deadline = None;
    let mut controls_finished = false;
    let result = async {
    loop {
        if !resize_pending {
            if let Some((watermark, protocol)) =
                live_scene.present(dimensions, terminal, &mut presenter)?
            {
                displayed_watermark = Some(watermark);
                modes = Some(protocol);
            } else {
                // Never route input using a scene from the previous geometry.
                modes = None;
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
            error = &mut stop => return Err(error),
            result = &mut controls => {
                controls_finished = true;
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
                live_scene.accept(frame.context("presentation receiver ended")??, &reference)?;
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
            event = terminal.next_event(), if modes.is_some() && !live_scene.input_suspended() && !observed.as_ref().is_some_and(|status| status.target_exited || status.terminal_session_ended) => {
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
    }.await;
    // Poison is already a failure, but must not bypass best-effort epoch close.
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .shutdown();
    completed.close();
    wake.notify_one();
    if !controls_finished {
        let cleanup = tokio::time::timeout(Duration::from_secs(10), &mut controls)
            .await
            .context("control shutdown timed out; outstanding effects uncertain; no retry")
            .and_then(|value| value);
        if let Err(cleanup) = cleanup {
            return match result {
                Ok(_) => Err(cleanup),
                Err(error) => {
                    Err(error.context(format!("control cleanup also failed: {cleanup:#}")))
                }
            };
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseButton};
    use std::io::Write;

    fn live_frame() -> bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::LiveFrame {
        use bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::{
            LiveCursor, LiveFrame,
        };
        let mut grid = TerminalGridStream::new(4, 2, GridLimits::default()).unwrap();
        grid.process(b"hi");
        let snapshot = grid.snapshot(0, 2);
        let execution = pane::TerminalExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        };
        LiveFrame {
            cursor: LiveCursor {
                source_epoch: execution.execution_token,
                source_reset_revision: 0,
                execution: execution.clone(),
                grid_revision: snapshot.revision,
                geometry_revision: 1,
                projected_raw_watermark: 2,
                durable_raw_watermark: 0,
                target_exited: false,
                output_drained: false,
                terminal_session_ended: false,
                recovery_required: false,
            },
            screen: Some(pane::TerminalExecutionScreen {
                execution,
                output_watermark: 2,
                cols: 4,
                rows: 2,
                cursor_col: 0,
                cursor_row: 0,
                cursor_visible: true,
                text: "hi".into(),
                encoded_grid: serde_json::to_vec(&snapshot).unwrap(),
            }),
        }
    }

    #[test]
    fn live_watermark_and_lifecycle_only_updates_retain_scene() {
        let frame = live_frame();
        let reference = frame.cursor.execution.clone();
        let mut scene = LiveScene::default();
        scene.accept(frame.clone(), &reference).unwrap();
        let retained = serde_json::to_vec(scene.snapshot.as_ref().unwrap()).unwrap();
        let revision = (
            frame.cursor.source_reset_revision,
            frame.cursor.grid_revision,
            frame.cursor.geometry_revision,
        );
        scene.displayed = Some(revision);
        let mut progress = frame;
        progress.screen = None;
        progress.cursor.projected_raw_watermark = 4;
        progress.cursor.durable_raw_watermark = 1;
        progress.cursor.target_exited = true;
        scene.accept(progress.clone(), &reference).unwrap();
        assert_eq!(scene.displayed, Some(revision));
        assert_eq!(
            serde_json::to_vec(scene.snapshot.as_ref().unwrap()).unwrap(),
            retained
        );
        assert_eq!(scene.cursor, Some(progress.cursor));
        assert!(scene.input_suspended());
        // A projected tail and target exit do not fabricate durable process
        // status, capture EOF, cleanup, or a durable recording receipt.
        assert!(!post_exit_cleanup_complete(&post_exit_client(None).status));
    }

    #[test]
    fn live_changed_scene_requires_screen_and_rejects_stale_source() {
        let frame = live_frame();
        let reference = frame.cursor.execution.clone();
        for kind in 0..7 {
            let mut scene = LiveScene::default();
            scene.accept(frame.clone(), &reference).unwrap();
            let before = scene.cursor.clone();
            let mut invalid = frame.clone();
            match kind {
                0 => {
                    invalid.screen = None;
                    invalid.cursor.grid_revision += 1;
                }
                1 => {
                    invalid.screen = None;
                    invalid.cursor.geometry_revision += 1;
                }
                2 => invalid.cursor.source_epoch = Uuid::new_v4(),
                3 => invalid.cursor.execution.execution_token = Uuid::new_v4(),
                4 => invalid.cursor.grid_revision += 1,
                5 => invalid.cursor.projected_raw_watermark += 1,
                _ => invalid.cursor.recovery_required = true,
            }
            assert!(scene.accept(invalid, &reference).is_err());
            assert_eq!(scene.cursor, before);
            assert!(scene.displayed.is_none());
        }
        let mut missing = frame;
        missing.screen = None;
        assert!(LiveScene::default().accept(missing, &reference).is_err());
    }

    #[test]
    fn live_changed_grid_accepts_matching_canonical_snapshot() {
        let frame = live_frame();
        let reference = frame.cursor.execution.clone();
        let mut scene = LiveScene::default();
        scene.accept(frame.clone(), &reference).unwrap();
        let mut changed = frame;
        let screen = changed.screen.as_mut().unwrap();
        let mut grid = TerminalGridStream::from_snapshot(
            scene.snapshot.as_ref().unwrap(),
            GridLimits::default(),
        )
        .unwrap();
        grid.process(b"!");
        let snapshot = grid.snapshot(0, 2);
        changed.cursor.grid_revision = snapshot.revision;
        changed.cursor.projected_raw_watermark += 1;
        screen.output_watermark = changed.cursor.projected_raw_watermark;
        screen.encoded_grid = serde_json::to_vec(&snapshot).unwrap();
        scene.accept(changed.clone(), &reference).unwrap();
        assert_eq!(scene.cursor, Some(changed.cursor));
        assert_eq!(scene.snapshot.as_ref().unwrap().revision, snapshot.revision);
        assert!(scene.displayed.is_none());
    }

    #[test]
    fn live_source_reset_requires_repair_and_successful_display_before_input() {
        struct FailedFlush;
        impl std::io::Write for FailedFlush {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("flush failed"))
            }
        }
        let frame = live_frame();
        let reference = frame.cursor.execution.clone();
        let mut scene = LiveScene::default();
        let mut presenter = UndecoratedGridPresenter::new(4, 2);
        let mut bytes = Vec::new();
        scene.accept(frame.clone(), &reference).unwrap();
        scene.present((4, 2), &mut bytes, &mut presenter).unwrap();
        assert!(!scene.input_suspended());
        let displayed = scene.displayed;
        let mut reset = frame.clone();
        reset.cursor.source_reset_revision += 1;
        // Parser replacement can restart grid/geometry revisions without
        // changing execution identity or the ordered raw-output position.
        let grid = TerminalGridStream::new(4, 2, GridLimits::default()).unwrap();
        let snapshot = grid.snapshot(0, 2);
        reset.cursor.grid_revision = snapshot.revision;
        reset.cursor.geometry_revision = 0;
        reset.screen.as_mut().unwrap().encoded_grid = serde_json::to_vec(&snapshot).unwrap();
        let mut missing = reset.clone();
        missing.screen = None;
        assert!(scene.accept(missing, &reference).is_err());
        assert_eq!(scene.displayed, displayed);
        scene.accept(reset.clone(), &reference).unwrap();
        assert!(scene.input_suspended());
        assert_eq!(scene.displayed, displayed);
        assert!(
            scene
                .present((5, 2), &mut bytes, &mut presenter)
                .unwrap()
                .is_none()
        );
        assert!(scene.input_suspended());
        assert!(
            scene
                .present((4, 2), &mut FailedFlush, &mut presenter)
                .is_err()
        );
        assert!(scene.input_suspended());
        assert_eq!(scene.displayed, displayed);
        scene.present((4, 2), &mut bytes, &mut presenter).unwrap();
        assert!(!scene.input_suspended());
        assert_eq!(
            scene.displayed,
            Some((
                reset.cursor.source_reset_revision,
                reset.cursor.grid_revision,
                reset.cursor.geometry_revision,
            ))
        );
        assert!(
            scene.accept(frame, &reference).is_err(),
            "old reset generation must not return"
        );
    }

    #[test]
    fn live_cursor_cannot_regress_or_resurrect_input() {
        let frame = live_frame();
        let reference = frame.cursor.execution.clone();
        let mut scene = LiveScene::default();
        scene.accept(frame.clone(), &reference).unwrap();
        let mut exited = frame.clone();
        exited.screen = None;
        exited.cursor.target_exited = true;
        exited.cursor.durable_raw_watermark = 2;
        scene.accept(exited, &reference).unwrap();
        assert!(scene.accept(frame, &reference).is_err());
        assert!(scene.input_suspended());
    }

    struct HumanClient {
        stream: bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputStream,
        calls: Vec<String>,
        fail_push: bool,
        bad_ack: bool,
    }

    impl bmux_plugin_sdk::TypedDispatchClient for HumanClient {
        fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> impl std::future::Future<Output = bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>>>
        {
            let mut payload = Some(payload);
            std::future::poll_fn(move |_| {
                let payload = payload.take().expect("transport polled after completion");
                std::task::Poll::Ready({
                    use bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1 as human;
                    use bmux_recording_plugin_api::recording_process_human_input_v1::client;
                    self.calls.push(operation.into());
                    let bytes = match operation {
                        "capabilities" => bmux_plugin_sdk::encode_service_message(
                            &human::HumanInputCapabilities {
                                protocol_version: 1,
                                max_payload_bytes: 16_384,
                                max_active_streams: 32,
                                dedup_window: 32,
                            },
                        ),
                        "open" => {
                            bmux_plugin_sdk::encode_service_message(
                                &Ok::<_, human::HumanInputError>(self.stream.clone()),
                            )
                        }
                        "push" => {
                            let request = bmux_plugin_sdk::decode_service_message::<
                                client::PushRequest,
                            >(&payload)
                            .unwrap();
                            assert_eq!(request.stream, self.stream);
                            assert_eq!(request.sequence, 1);
                            if self.fail_push {
                                bmux_plugin_sdk::encode_service_message(&Err::<
                                    human::HumanInputAck,
                                    _,
                                >(
                                    human::HumanInputError::Uncertain {
                                        reason: "PTY acknowledgment lost".into(),
                                    },
                                ))
                            } else {
                                bmux_plugin_sdk::encode_service_message(&Ok::<
                                    _,
                                    human::HumanInputError,
                                >(
                                    human::HumanInputAck {
                                        stream: self.stream.clone(),
                                        sequence: if self.bad_ack { 2 } else { request.sequence },
                                        written_bytes: u64::try_from(request.data.len()).unwrap(),
                                    },
                                ))
                            }
                        }
                        "close" => {
                            bmux_plugin_sdk::encode_service_message(
                                &Ok::<_, human::HumanInputError>(()),
                            )
                        }
                        unexpected => panic!(
                            "unexpected operation: {unexpected}; no durable fallback or retry allowed"
                        ),
                    };
                    Ok(bytes.unwrap())
                })
            })
        }
    }

    fn human_client() -> HumanClient {
        HumanClient {
            stream: bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputStream {
                execution: pane::TerminalExecutionRef {
                    session_id: Uuid::new_v4(),
                    pane_id: Uuid::new_v4(),
                    execution_token: Uuid::new_v4(),
                },
                stream_id: Uuid::new_v4(),
            },
            calls: Vec::new(),
            fail_push: false,
            bad_ack: false,
        }
    }

    #[tokio::test]
    async fn human_controls_close_on_uncertain_and_invalid_ack_without_retry() {
        for bad_ack in [false, true] {
            let mut client = human_client();
            client.fail_push = !bad_ack;
            client.bad_ack = bad_ack;
            let queue = std::sync::Arc::new(std::sync::Mutex::new(ControlQueue::default()));
            queue
                .lock()
                .unwrap()
                .push(ControlCommand::Input(vec![1]))
                .unwrap();
            let reference = client.stream.execution.clone();
            let (completed, _receiver) = tokio::sync::mpsc::channel(1);
            let result = drive_controls(
                &mut client,
                Uuid::new_v4(),
                &reference,
                &queue,
                &tokio::sync::Notify::new(),
                completed,
            )
            .await;
            assert!(result.is_err());
            assert_eq!(client.calls, ["capabilities", "open", "push", "close"]);
        }
    }

    #[tokio::test]
    async fn human_controls_close_on_identity_mismatch_and_each_clean_shutdown() {
        let mut client = human_client();
        let queue = std::sync::Arc::new(std::sync::Mutex::new(ControlQueue::default()));
        let mut wrong = client.stream.execution.clone();
        wrong.execution_token = Uuid::new_v4();
        let (completed, _receiver) = tokio::sync::mpsc::channel(1);
        assert!(
            drive_controls(
                &mut client,
                Uuid::new_v4(),
                &wrong,
                &queue,
                &tokio::sync::Notify::new(),
                completed,
            )
            .await
            .is_err()
        );
        assert_eq!(client.calls, ["capabilities", "open", "close"]);
        for _ in 0..2 {
            client.calls.clear();
            queue.lock().unwrap().shutdown();
            let reference = client.stream.execution.clone();
            let (completed, _receiver) = tokio::sync::mpsc::channel(1);
            drive_controls(
                &mut client,
                Uuid::new_v4(),
                &reference,
                &queue,
                &tokio::sync::Notify::new(),
                completed,
            )
            .await
            .unwrap();
            assert_eq!(client.calls, ["capabilities", "open", "close"]);
        }
    }

    #[tokio::test]
    async fn human_completion_disconnect_closes_stream_after_single_write() {
        let mut client = human_client();
        let queue = std::sync::Arc::new(std::sync::Mutex::new(ControlQueue::default()));
        queue
            .lock()
            .unwrap()
            .push(ControlCommand::Input(vec![1]))
            .unwrap();
        let (completed, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let reference = client.stream.execution.clone();
        drive_controls(
            &mut client,
            Uuid::new_v4(),
            &reference,
            &queue,
            &tokio::sync::Notify::new(),
            completed,
        )
        .await
        .unwrap();
        assert_eq!(client.calls, ["capabilities", "open", "push", "close"]);
    }

    #[test]
    fn control_shutdown_discards_queued_input_and_never_resumes_it() {
        let mut queue = ControlQueue::default();
        queue.push(ControlCommand::Input(vec![1])).unwrap();
        queue.push(ControlCommand::Resize((80, 24))).unwrap();
        queue.shutdown();
        assert_eq!(queue.bytes, 0);
        assert!(queue.commands.is_empty());
        assert!(queue.take_batch().is_none());
        assert!(queue.push(ControlCommand::Input(vec![2])).is_err());
    }

    struct PostExitClient {
        status: process::RunStatus,
        eof_delay: Option<Duration>,
        wait_calls: usize,
        status_calls: usize,
    }

    impl bmux_plugin_sdk::TypedDispatchClient for PostExitClient {
        async fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>> {
            let before = self.status.clone();
            match operation {
                "status" => {
                    let request = bmux_plugin_sdk::decode_service_message::<
                        process::client::StatusRequest,
                    >(&payload)
                    .unwrap();
                    assert_eq!(request.run_id, self.status.run_id);
                    self.status_calls += 1;
                }
                "wait" => {
                    let request = bmux_plugin_sdk::decode_service_message::<
                        process::client::WaitRequest,
                    >(&payload)
                    .unwrap();
                    assert_eq!(request.run_id, self.status.run_id);
                    self.wait_calls += 1;
                    let eof = async {
                        if let Some(delay) = self.eof_delay {
                            tokio::time::sleep(delay).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    };
                    if tokio::time::timeout(
                        Duration::from_millis(u64::from(request.timeout_ms)),
                        eof,
                    )
                    .await
                    .is_ok()
                    {
                        self.status.capture_eof = true;
                        self.status.output_drained = true;
                        self.status.cleanup_complete = true;
                    }
                }
                _ => panic!("must not terminate or finish: {operation}"),
            }
            // Deliberately return a pre-EOF wait snapshot: the explicit final
            // status refresh must obtain the current authoritative flags.
            Ok(
                bmux_plugin_sdk::encode_service_message(&Ok::<_, process::ProcessError>(
                    if operation == "wait" {
                        before
                    } else {
                        self.status.clone()
                    },
                ))
                .unwrap(),
            )
        }
    }

    fn post_exit_client(eof_delay: Option<Duration>) -> PostExitClient {
        PostExitClient {
            status: process::RunStatus {
                schema_version: 4,
                run_id: Uuid::new_v4(),
                revision: 1,
                launch_mutation_id: Uuid::new_v4(),
                recording_id: Uuid::new_v4(),
                execution: None,
                runtime_root: "/isolated/runtime".into(),
                phase: process::RunPhase::Exited,
                exit_code: Some(0),
                target_exited: true,
                terminal_session_ended: true,
                capture_eof: false,
                capture_watermark: 92,
                recording_watermark: 92,
                durable_recording_complete: false,
                timing_complete: true,
                cleanup_complete: false,
                recovery_required: false,
                output_drained: false,
                output_watermark: 92,
                capture_finalized: false,
                errors: vec![],
            },
            eof_delay,
            wait_calls: 0,
            status_calls: 0,
        }
    }

    #[tokio::test]
    async fn post_exit_cleanup_accepts_real_eof_beyond_presentation_grace() {
        let mut client = post_exit_client(Some(POST_EXIT_DRAIN_GRACE + Duration::from_millis(50)));
        let run_id = client.status.run_id;
        let started = tokio::time::Instant::now();
        let status = wait_for_post_exit_cleanup(&mut client, run_id, 2000)
            .await
            .unwrap();
        assert!(started.elapsed() > POST_EXIT_DRAIN_GRACE);
        assert!(post_exit_cleanup_complete(&status));
        assert_eq!(client.wait_calls, 1);
        assert_eq!(client.status_calls, 2);
        assert!(!status.capture_finalized);
    }

    #[test]
    fn post_exit_cleanup_requires_each_authoritative_completion_flag() {
        let mut status = post_exit_client(None).status;
        status.capture_eof = true;
        status.output_drained = true;
        status.cleanup_complete = true;
        assert!(post_exit_cleanup_complete(&status));
        status.capture_eof = false;
        assert!(!post_exit_cleanup_complete(&status));
        status.capture_eof = true;
        status.output_drained = false;
        assert!(!post_exit_cleanup_complete(&status));
        status.output_drained = true;
        status.cleanup_complete = false;
        assert!(!post_exit_cleanup_complete(&status));
    }

    #[tokio::test]
    async fn post_exit_cleanup_refuses_real_no_eof_deadline() {
        // Model an escaped slave holder: target/session ended, but EOF never
        // occurs. The owner's real timeout returns pending, not completion.
        let mut client = post_exit_client(None);
        let run_id = client.status.run_id;
        let deadline = Duration::from_millis(40);
        let started = tokio::time::Instant::now();
        let error = wait_for_post_exit_cleanup(&mut client, run_id, 40)
            .await
            .unwrap_err();
        assert!(started.elapsed() >= deadline);
        assert!(error.to_string().contains("capture/cleanup pending"));
        assert_eq!(client.wait_calls, 1);
        assert_eq!(client.status_calls, 2);
        assert!(!client.status.capture_eof);
        assert!(!client.status.output_drained);
        assert!(!client.status.cleanup_complete);
        assert!(!client.status.capture_finalized);
    }

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
        let error = encode_input(
            key(KeyCode::Char('z'), KeyModifiers::CONTROL),
            InputModes::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("suspension is unsupported"));
        assert_eq!(failure_kind(&error), InteractiveFailureKind::Unsupported);
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
