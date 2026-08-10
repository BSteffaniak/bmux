use anyhow::{Context, Result};
use bmux_recording_protocol::{
    DisplayActivityKind, DisplayCursorShape, DisplayTrackEnvelope, DisplayTrackEvent, write_frame,
};
use crossterm::terminal;
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod terminal_profile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorVisualShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy)]
struct CursorReplayState {
    shape: CursorVisualShape,
    blink_enabled: bool,
}

impl Default for CursorReplayState {
    fn default() -> Self {
        Self {
            shape: CursorVisualShape::Block,
            blink_enabled: true,
        }
    }
}

fn update_cursor_replay_state(state: &mut CursorReplayState, data: &[u8]) {
    let mut index = 0usize;
    while index + 4 < data.len() {
        if data[index] != 0x1b || data[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let mut cursor = index + 2;
        let mut value: u16 = 0;
        let mut saw_digit = false;
        while cursor < data.len() && data[cursor].is_ascii_digit() {
            saw_digit = true;
            value = value
                .saturating_mul(10)
                .saturating_add(u16::from(data[cursor].saturating_sub(b'0')));
            cursor += 1;
        }
        if cursor + 1 >= data.len() || data[cursor] != b' ' || data[cursor + 1] != b'q' {
            index += 1;
            continue;
        }
        let ps = if saw_digit { value } else { 0 };
        match ps {
            0 | 1 => {
                state.shape = CursorVisualShape::Block;
                state.blink_enabled = true;
            }
            2 => {
                state.shape = CursorVisualShape::Block;
                state.blink_enabled = false;
            }
            3 => {
                state.shape = CursorVisualShape::Underline;
                state.blink_enabled = true;
            }
            4 => {
                state.shape = CursorVisualShape::Underline;
                state.blink_enabled = false;
            }
            5 => {
                state.shape = CursorVisualShape::Bar;
                state.blink_enabled = true;
            }
            6 => {
                state.shape = CursorVisualShape::Bar;
                state.blink_enabled = false;
            }
            _ => {}
        }
        index = cursor + 2;
    }
}

const fn display_cursor_shape_from_visual(shape: CursorVisualShape) -> DisplayCursorShape {
    match shape {
        CursorVisualShape::Block => DisplayCursorShape::Block,
        CursorVisualShape::Bar => DisplayCursorShape::Bar,
        CursorVisualShape::Underline => DisplayCursorShape::Underline,
    }
}

#[derive(Clone, Copy, Debug)]
struct CellMetrics {
    width: u16,
    height: u16,
}

fn infer_cell_metrics(
    window_width: u16,
    window_height: u16,
    cols: u16,
    rows: u16,
) -> Option<CellMetrics> {
    if window_width == 0 || window_height == 0 || cols == 0 || rows == 0 {
        return None;
    }
    let width = (window_width / cols).max(1);
    let height = (window_height / rows).max(1);
    Some(CellMetrics { width, height })
}

fn capture_stream_open_metrics() -> (Option<u16>, Option<u16>, Option<u16>, Option<u16>) {
    let (window_width_px, window_height_px) =
        terminal::window_size().ok().map_or((None, None), |value| {
            (
                (value.width > 0).then_some(value.width),
                (value.height > 0).then_some(value.height),
            )
        });

    let (cell_width_px, cell_height_px) = terminal::size()
        .ok()
        .and_then(|(cols, rows)| {
            let window_width = window_width_px?;
            let window_height = window_height_px?;
            infer_cell_metrics(window_width, window_height, cols, rows)
        })
        .map_or((None, None), |value| {
            (Some(value.width), Some(value.height))
        });

    (
        cell_width_px,
        cell_height_px,
        window_width_px,
        window_height_px,
    )
}

// Display track types are defined in bmux_ipc for cross-module sharing.

const DISPLAY_CAPTURE_QUEUE_CAPACITY: usize = 4096;
const DISPLAY_CAPTURE_SEGMENT_MAX_AGE: Duration = Duration::from_secs(2);
const DISPLAY_CAPTURE_PRUNE_GRACE: Duration = Duration::from_secs(5);
pub(super) struct DisplayCaptureWriter {
    sender: mpsc::SyncSender<DisplayCaptureCommand>,
    queued_commands: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
    dropped_events: u64,
}

enum DisplayCaptureCommand {
    Event(DisplayTrackEvent),
    Frame {
        data: Arc<[u8]>,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
        cursor_changed: bool,
    },
    Flush(mpsc::Sender<Result<()>>),
    Close(mpsc::Sender<Result<()>>),
}

/// Outcome of writing one event into the current segment.
///
/// An empty image update for an already-empty screen writes nothing, and a
/// segment that received no bytes must not age into a rotation, so the caller
/// needs to distinguish the two cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordOutcome {
    Written,
    Skipped,
}

/// Writes display events into the current segment file.
///
/// This type deliberately owns no rotation state: without a rolling window,
/// segment index, or segment clock there is nothing for a segment-level write
/// to rotate. That makes the "baseline writes must not rotate" invariant a
/// property of the type rather than something callers must remember.
struct DisplaySegmentWriter {
    writer: BufWriter<std::fs::File>,
    stream_opened_baseline: DisplayTrackEvent,
    latest_resize: Option<(u16, u16)>,
    replay_grid: bmux_terminal_grid::TerminalGridStream,
    cursor_replay_state: CursorReplayState,
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    last_image_count: usize,
}

/// Owns the capture clock, segment rotation, and pruning, and delegates every
/// write to [`DisplaySegmentWriter`].
struct DisplayCaptureFileWriter {
    segment: DisplaySegmentWriter,
    recording_path: PathBuf,
    client_id: Uuid,
    rolling_window: Option<Duration>,
    started_at: Instant,
    segment_index: u64,
    segment_start_ns: u64,
    closed_segments: VecDeque<(PathBuf, u64)>,
}

impl DisplayCaptureWriter {
    /// Create a display capture writer backed by a dedicated OS thread.  The
    /// attach loop only enqueues events; disk writes, rotation, and pruning are
    /// kept off the interactive hot path.
    pub(super) fn open(
        recording_id: Uuid,
        recording_path: &Path,
        client_id: Uuid,
        rolling_window_secs: Option<u64>,
    ) -> Result<Self> {
        let mut writer = DisplayCaptureFileWriter::open(
            recording_id,
            recording_path,
            client_id,
            rolling_window_secs.map(Duration::from_secs),
        )?;
        writer.record_stream_opened()?;
        let (sender, receiver) = mpsc::sync_channel(DISPLAY_CAPTURE_QUEUE_CAPACITY);
        let queued_commands = Arc::new(AtomicUsize::new(0));
        let worker_queued_commands = Arc::clone(&queued_commands);
        let worker = thread::Builder::new()
            .name(format!("bmux-display-capture-{recording_id}"))
            .spawn(move || {
                display_capture_writer_loop(&mut writer, receiver, &worker_queued_commands);
            })
            .context("failed spawning display capture writer thread")?;
        Ok(Self {
            sender,
            queued_commands,
            worker: Some(worker),
            dropped_events: 0,
        })
    }

    pub(super) fn record_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::Event(DisplayTrackEvent::Resize {
            cols,
            rows,
        }))
    }

    pub(super) fn has_frame_capacity(&self) -> bool {
        self.queued_commands.load(Ordering::Relaxed) < DISPLAY_CAPTURE_QUEUE_CAPACITY
    }

    pub(super) fn record_frame(
        &mut self,
        data: Arc<[u8]>,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
        cursor_changed: bool,
    ) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if !self.has_frame_capacity() {
            self.record_dropped_event();
            return Ok(());
        }
        self.enqueue(DisplayCaptureCommand::Frame {
            data,
            cursor_state,
            cursor_changed,
        })
    }

    pub(super) fn record_activity(&mut self, kind: DisplayActivityKind) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::Event(DisplayTrackEvent::Activity {
            kind,
        }))
    }

    pub(super) fn record_stream_closed(&mut self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.queued_commands.fetch_add(1, Ordering::Relaxed);
        if let Err(error) = self.sender.send(DisplayCaptureCommand::Close(sender)) {
            self.queued_commands.fetch_sub(1, Ordering::Relaxed);
            return Err(error).context("display capture writer is closed");
        }
        let result = receiver
            .recv()
            .context("display capture writer closed without acknowledgement")?;
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            return Err(anyhow::anyhow!("display capture writer thread panicked"));
        }
        result
    }

    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub(super) fn record_images(
        &mut self,
        images: &[bmux_attach_image_protocol::AttachPaneImage],
    ) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::Event(
            DisplayTrackEvent::ImageUpdate {
                images: images.to_vec(),
            },
        ))
    }

    pub(super) fn flush(&self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.queued_commands.fetch_add(1, Ordering::Relaxed);
        if let Err(error) = self.sender.send(DisplayCaptureCommand::Flush(sender)) {
            self.queued_commands.fetch_sub(1, Ordering::Relaxed);
            return Err(error).context("display capture writer is closed");
        }
        receiver
            .recv()
            .context("display capture writer closed without flushing")?
    }

    fn enqueue(&mut self, command: DisplayCaptureCommand) -> Result<()> {
        self.queued_commands.fetch_add(1, Ordering::Relaxed);
        match self.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                self.queued_commands.fetch_sub(1, Ordering::Relaxed);
                self.record_dropped_event();
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.queued_commands.fetch_sub(1, Ordering::Relaxed);
                Err(anyhow::anyhow!("display capture writer is closed"))
            }
        }
    }

    fn record_dropped_event(&mut self) {
        self.dropped_events = self.dropped_events.saturating_add(1);
        if self.dropped_events == 1 || self.dropped_events.is_multiple_of(1024) {
            tracing::warn!(
                dropped_events = self.dropped_events,
                "display capture queue is full; dropping recording display events"
            );
        }
    }
}

impl Drop for DisplayCaptureWriter {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.record_stream_closed();
        }
    }
}

impl DisplaySegmentWriter {
    /// Swap in a new output file, flushing whatever is still buffered for the
    /// outgoing one first.
    ///
    /// Flushing is folded into the swap so buffered bytes for a closed segment
    /// cannot be dropped by reordering the two steps at a call site. Replay
    /// state (grid, cursor, last resize) is intentionally preserved so the next
    /// segment can emit a self-contained baseline.
    fn replace_output(&mut self, file: std::fs::File) -> Result<()> {
        self.flush()?;
        self.writer = BufWriter::new(file);
        Ok(())
    }

    /// Write the events that let a replayer start from this segment alone: the
    /// stream-opened metadata, the last known size, and a full repaint.
    ///
    /// All three share `mono_ns` so a segment's opening events carry the exact
    /// segment boundary timestamp instead of drifting later clock reads.
    fn write_baseline(&mut self, mono_ns: u64) -> Result<()> {
        self.write(self.stream_opened_baseline.clone(), mono_ns)?;
        if let Some((cols, rows)) = self.latest_resize {
            self.write(DisplayTrackEvent::Resize { cols, rows }, mono_ns)?;
        }
        let repaint = bmux_terminal_grid::full_screen_repaint_bytes(self.replay_grid.grid());
        if !repaint.is_empty() {
            self.write(DisplayTrackEvent::FrameBytes { data: repaint }, mono_ns)?;
        }
        Ok(())
    }

    fn write_cursor_snapshot(
        &mut self,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
        mono_ns: u64,
    ) -> Result<RecordOutcome> {
        let (x, y, visible) =
            cursor_state.map_or((0, 0, false), |state| (state.x, state.y, state.visible));
        self.write(
            DisplayTrackEvent::CursorSnapshot {
                x,
                y,
                visible,
                shape: display_cursor_shape_from_visual(self.cursor_replay_state.shape),
                blink_enabled: self.cursor_replay_state.blink_enabled,
            },
            mono_ns,
        )
    }

    fn write(&mut self, event: DisplayTrackEvent, mono_ns: u64) -> Result<RecordOutcome> {
        if let DisplayTrackEvent::Resize { cols, rows } = &event
            && *cols > 0
            && *rows > 0
        {
            self.latest_resize = Some((*cols, *rows));
            let _ = self.replay_grid.resize(*cols, *rows);
        }
        if let DisplayTrackEvent::FrameBytes { data } = &event {
            update_cursor_replay_state(&mut self.cursor_replay_state, data);
            self.replay_grid.process(data);
        }
        #[cfg(any(
            feature = "image-sixel",
            feature = "image-kitty",
            feature = "image-iterm2"
        ))]
        if let DisplayTrackEvent::ImageUpdate { images } = &event {
            let count = images.len();
            if count == 0 && self.last_image_count == 0 {
                return Ok(RecordOutcome::Skipped);
            }
            self.last_image_count = count;
        }
        let envelope = DisplayTrackEnvelope { mono_ns, event };
        write_frame(&mut self.writer, &envelope)
            .map_err(|e| anyhow::anyhow!("display track write_frame failed: {e}"))?;
        Ok(RecordOutcome::Written)
    }

    fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .context("failed flushing display capture writer")
    }
}

impl DisplayCaptureFileWriter {
    fn open(
        recording_id: Uuid,
        recording_path: &Path,
        client_id: Uuid,
        rolling_window: Option<Duration>,
    ) -> Result<Self> {
        std::fs::create_dir_all(recording_path).with_context(|| {
            format!(
                "failed creating recording path {}",
                recording_path.display()
            )
        })?;
        let display_track_path =
            display_track_output_path(recording_path, client_id, 0, rolling_window);
        let file = open_display_track_file(&display_track_path)?;
        let stream_opened_baseline = capture_stream_opened_event(recording_id, client_id);
        let latest_resize = current_terminal_size();
        let (initial_cols, initial_rows) = latest_resize.unwrap_or((80, 24));
        let replay_grid = bmux_terminal_grid::TerminalGridStream::new(
            initial_cols.max(1),
            initial_rows.max(1),
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("display capture replay grid dimensions are valid");
        Ok(Self {
            segment: DisplaySegmentWriter {
                writer: BufWriter::new(file),
                stream_opened_baseline,
                latest_resize,
                replay_grid,
                cursor_replay_state: CursorReplayState::default(),
                #[cfg(any(
                    feature = "image-sixel",
                    feature = "image-kitty",
                    feature = "image-iterm2"
                ))]
                last_image_count: 0,
            },
            recording_path: recording_path.to_path_buf(),
            client_id,
            rolling_window,
            started_at: Instant::now(),
            segment_index: 0,
            segment_start_ns: 0,
            closed_segments: VecDeque::new(),
        })
    }

    fn record_stream_opened(&mut self) -> Result<()> {
        self.segment.write_baseline(self.elapsed_ns())
    }

    fn elapsed_ns(&self) -> u64 {
        duration_nanos_u64(self.started_at.elapsed())
    }

    fn record_cursor_snapshot(
        &mut self,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
    ) -> Result<()> {
        let mono_ns = self.elapsed_ns();
        if self.segment.write_cursor_snapshot(cursor_state, mono_ns)? == RecordOutcome::Written {
            self.maybe_rotate(mono_ns)?;
        }
        Ok(())
    }

    /// Record one externally queued event, then consider rotation exactly once.
    ///
    /// This is the only path that can rotate: baseline writes go straight to
    /// [`DisplaySegmentWriter`], which cannot reach rotation at all.
    fn record(&mut self, event: DisplayTrackEvent) -> Result<()> {
        let mono_ns = self.elapsed_ns();
        if self.segment.write(event, mono_ns)? == RecordOutcome::Written {
            self.maybe_rotate(mono_ns)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.segment.flush()
    }

    fn maybe_rotate(&mut self, mono_ns: u64) -> Result<()> {
        if self.rolling_window.is_none() {
            return Ok(());
        }
        let segment_age = Duration::from_nanos(mono_ns.saturating_sub(self.segment_start_ns));
        if segment_age < DISPLAY_CAPTURE_SEGMENT_MAX_AGE {
            return Ok(());
        }
        self.rotate(mono_ns)
    }

    fn rotate(&mut self, end_ns: u64) -> Result<()> {
        let old_path =
            display_track_segment_path(&self.recording_path, self.client_id, self.segment_index);
        self.closed_segments.push_back((old_path, end_ns));
        self.segment_index = self.segment_index.saturating_add(1);
        self.segment_start_ns = end_ns;
        let new_path =
            display_track_segment_path(&self.recording_path, self.client_id, self.segment_index);
        self.segment
            .replace_output(open_display_track_file(&new_path)?)?;
        self.segment.write_baseline(end_ns)?;
        self.prune_closed_segments(end_ns)
    }

    fn prune_closed_segments(&mut self, now_ns: u64) -> Result<()> {
        let Some(window) = self.rolling_window else {
            return Ok(());
        };
        let retention = window.saturating_add(DISPLAY_CAPTURE_PRUNE_GRACE);
        let cutoff_ns = now_ns.saturating_sub(duration_nanos_u64(retention));
        while self
            .closed_segments
            .front()
            .is_some_and(|(_, end_ns)| *end_ns < cutoff_ns)
        {
            let Some((path, _)) = self.closed_segments.pop_front() else {
                break;
            };
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error).with_context(|| {
                    format!(
                        "failed removing old display track segment {}",
                        path.display()
                    )
                });
            }
        }
        Ok(())
    }
}

fn display_capture_writer_loop(
    writer: &mut DisplayCaptureFileWriter,
    receiver: mpsc::Receiver<DisplayCaptureCommand>,
    queued_commands: &AtomicUsize,
) {
    for command in receiver {
        queued_commands.fetch_sub(1, Ordering::Relaxed);
        match command {
            DisplayCaptureCommand::Event(event) => {
                if let Err(error) = writer.record(event) {
                    tracing::warn!(error = %error, "display capture write failed");
                }
            }
            DisplayCaptureCommand::Frame {
                data,
                cursor_state,
                cursor_changed,
            } => {
                if let Err(error) = writer.record(DisplayTrackEvent::FrameBytes {
                    data: data.to_vec(),
                }) {
                    tracing::warn!(error = %error, "display capture write failed");
                }
                if let Err(error) = writer.record(DisplayTrackEvent::Activity {
                    kind: DisplayActivityKind::Output,
                }) {
                    tracing::warn!(error = %error, "display capture activity write failed");
                }
                if let Err(error) = writer.record_cursor_snapshot(cursor_state) {
                    tracing::warn!(error = %error, "display capture cursor snapshot failed");
                }
                if cursor_changed
                    && let Err(error) = writer.record(DisplayTrackEvent::Activity {
                        kind: DisplayActivityKind::Cursor,
                    })
                {
                    tracing::warn!(error = %error, "display capture cursor activity write failed");
                }
            }
            DisplayCaptureCommand::Flush(ack) => {
                let _ = ack.send(writer.flush());
            }
            DisplayCaptureCommand::Close(ack) => {
                let result = writer
                    .record(DisplayTrackEvent::StreamClosed)
                    .and_then(|()| writer.flush());
                let _ = ack.send(result);
                break;
            }
        }
    }
}

fn capture_stream_opened_event(recording_id: Uuid, client_id: Uuid) -> DisplayTrackEvent {
    let (cell_width_px, cell_height_px, window_width_px, window_height_px) =
        capture_stream_open_metrics();
    let terminal_profile = terminal_profile::detect_render_profile();
    let terminal_profile_bytes = terminal_profile
        .as_ref()
        .and_then(|p| bmux_ipc::encode(p).ok());
    DisplayTrackEvent::StreamOpened {
        client_id,
        recording_id,
        cell_width_px,
        cell_height_px,
        window_width_px,
        window_height_px,
        terminal_profile: terminal_profile_bytes,
    }
}

fn current_terminal_size() -> Option<(u16, u16)> {
    let Ok((cols, rows)) = terminal::size() else {
        return None;
    };
    (cols > 0 && rows > 0).then_some((cols, rows))
}

fn open_display_track_file(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed opening display track {}", path.display()))
}

fn duration_nanos_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos().min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

fn display_track_path(recording_path: &Path, client_id: Uuid) -> PathBuf {
    recording_path.join(format!("display-{client_id}.bin"))
}

fn display_track_segment_path(recording_path: &Path, client_id: Uuid, index: u64) -> PathBuf {
    recording_path.join(format!("display-{client_id}.part{index}.bin"))
}

fn display_track_output_path(
    recording_path: &Path,
    client_id: Uuid,
    index: u64,
    rolling_window: Option<Duration>,
) -> PathBuf {
    if rolling_window.is_some() {
        display_track_segment_path(recording_path, client_id, index)
    } else {
        display_track_path(recording_path, client_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_recording_protocol::read_frames;

    const ROLLING_WINDOW: Duration = Duration::from_secs(10);
    const SECOND_NS: u64 = 1_000_000_000;

    fn rolling_writer(dir: &Path) -> (DisplayCaptureFileWriter, Uuid) {
        let client_id = Uuid::new_v4();
        let writer =
            DisplayCaptureFileWriter::open(Uuid::new_v4(), dir, client_id, Some(ROLLING_WINDOW))
                .expect("rolling display capture writer opens");
        (writer, client_id)
    }

    fn segment_events(path: &Path) -> Vec<DisplayTrackEnvelope> {
        let bytes = std::fs::read(path).expect("segment file is readable");
        let result =
            read_frames::<DisplayTrackEnvelope>(&bytes).expect("segment frames decode cleanly");
        assert_eq!(result.bytes_remaining, 0, "segment must not be truncated");
        result.frames
    }

    /// Rotation must advance by exactly one segment per call, and the new
    /// segment must open with its own baseline.
    #[test]
    fn rotate_advances_exactly_one_segment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut writer, client_id) = rolling_writer(dir.path());

        writer
            .rotate(50 * SECOND_NS)
            .expect("rotation past the segment age succeeds");

        assert_eq!(writer.segment_index, 1);
        assert_eq!(writer.closed_segments.len(), 1);
        assert_eq!(writer.segment_start_ns, 50 * SECOND_NS);
        assert!(display_track_segment_path(dir.path(), client_id, 1).exists());
    }

    /// A rotation baseline is stamped at the segment boundary, so seeking to a
    /// segment start yields a coherent stream-opened/resize/repaint prefix.
    #[test]
    fn segment_baseline_events_share_boundary_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut writer, client_id) = rolling_writer(dir.path());
        let boundary_ns = 7 * SECOND_NS;

        writer.rotate(boundary_ns).expect("rotation succeeds");
        writer.flush().expect("flush succeeds");

        let events = segment_events(&display_track_segment_path(dir.path(), client_id, 1));
        assert!(
            !events.is_empty(),
            "a rotated segment starts with a baseline"
        );
        for envelope in &events {
            assert_eq!(
                envelope.mono_ns, boundary_ns,
                "baseline events carry the segment boundary timestamp"
            );
        }
        assert!(matches!(
            events.first().map(|envelope| &envelope.event),
            Some(DisplayTrackEvent::StreamOpened { .. })
        ));
    }

    /// Aged events must rotate at most once each and prune old segments, so a
    /// long rolling-window capture stays bounded in segment count and disk use.
    ///
    /// This exercises the rotation/pruning policy, not the old recursion: the
    /// recursion is now prevented structurally (baseline writes go through
    /// [`DisplaySegmentWriter`], which cannot reach rotation), so there is no
    /// runtime path left for a test to drive into it.
    #[test]
    fn rolling_window_capture_stays_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut writer, client_id) = rolling_writer(dir.path());

        for step in 0..40_u64 {
            let mono_ns = step * 5 * SECOND_NS;
            let outcome = writer
                .segment
                .write(
                    DisplayTrackEvent::FrameBytes {
                        data: format!("frame {step}\r\n").into_bytes(),
                    },
                    mono_ns,
                )
                .expect("segment write succeeds");
            assert_eq!(outcome, RecordOutcome::Written);
            writer
                .maybe_rotate(mono_ns)
                .expect("rotation stays bounded across many segments");
        }
        writer.flush().expect("flush succeeds");

        assert_eq!(
            writer.segment_index, 39,
            "each aged event rotates at most once"
        );
        assert!(
            writer.closed_segments.len() < 5,
            "pruning bounds retained segments, got {}",
            writer.closed_segments.len()
        );
        assert!(
            !display_track_segment_path(dir.path(), client_id, 0).exists(),
            "segments older than the retention window are pruned"
        );
    }

    /// An empty image update for an already-empty screen writes nothing, so it
    /// must not age the segment into a rotation.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    #[test]
    fn empty_image_update_does_not_rotate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut writer, _client_id) = rolling_writer(dir.path());

        let outcome = writer
            .segment
            .write(
                DisplayTrackEvent::ImageUpdate { images: Vec::new() },
                50 * SECOND_NS,
            )
            .expect("empty image update succeeds");

        assert_eq!(outcome, RecordOutcome::Skipped);
        assert_eq!(
            writer.segment_index, 0,
            "a skipped write must not trigger rotation"
        );
    }
}
