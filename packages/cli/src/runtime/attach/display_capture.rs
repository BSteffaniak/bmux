use anyhow::{Context, Result};
use bmux_recording_protocol::{
    DisplayActivityKind, DisplayCursorShape, DisplayTrackEnvelope, DisplayTrackEvent, write_frame,
};
use crossterm::terminal;
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
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
    worker: Option<thread::JoinHandle<()>>,
    dropped_events: u64,
}

enum DisplayCaptureCommand {
    Event(DisplayTrackEvent),
    CursorSnapshot(Option<crate::runtime::attach::state::AttachCursorState>),
    Flush(mpsc::Sender<Result<()>>),
    Close(mpsc::Sender<Result<()>>),
}

struct DisplayCaptureFileWriter {
    recording_path: PathBuf,
    client_id: Uuid,
    rolling_window: Option<Duration>,
    started_at: Instant,
    writer: BufWriter<std::fs::File>,
    segment_index: u64,
    segment_start_ns: u64,
    closed_segments: VecDeque<(PathBuf, u64)>,
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
        let worker = thread::Builder::new()
            .name(format!("bmux-display-capture-{recording_id}"))
            .spawn(move || display_capture_writer_loop(&mut writer, receiver))
            .context("failed spawning display capture writer thread")?;
        Ok(Self {
            sender,
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

    pub(super) fn record_frame_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.enqueue(DisplayCaptureCommand::Event(
            DisplayTrackEvent::FrameBytes {
                data: data.to_vec(),
            },
        ))
    }

    pub(super) fn record_activity(&mut self, kind: DisplayActivityKind) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::Event(DisplayTrackEvent::Activity {
            kind,
        }))
    }

    pub(super) fn record_cursor_snapshot(
        &mut self,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
    ) -> Result<()> {
        self.enqueue(DisplayCaptureCommand::CursorSnapshot(cursor_state))
    }

    pub(super) fn record_stream_closed(&mut self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.sender
            .send(DisplayCaptureCommand::Close(sender))
            .context("display capture writer is closed")?;
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
        self.sender
            .send(DisplayCaptureCommand::Flush(sender))
            .context("display capture writer is closed")?;
        receiver
            .recv()
            .context("display capture writer closed without flushing")?
    }

    fn enqueue(&mut self, command: DisplayCaptureCommand) -> Result<()> {
        match self.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                self.dropped_events = self.dropped_events.saturating_add(1);
                if self.dropped_events == 1 || self.dropped_events.is_multiple_of(1024) {
                    tracing::warn!(
                        dropped_events = self.dropped_events,
                        "display capture queue is full; dropping recording display events"
                    );
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(anyhow::anyhow!("display capture writer is closed"))
            }
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
            recording_path: recording_path.to_path_buf(),
            client_id,
            rolling_window,
            started_at: Instant::now(),
            writer: BufWriter::new(file),
            segment_index: 0,
            segment_start_ns: 0,
            closed_segments: VecDeque::new(),
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
        })
    }

    fn record_stream_opened(&mut self) -> Result<()> {
        self.record_segment_baseline()
    }

    fn record_segment_baseline(&mut self) -> Result<()> {
        self.record(self.stream_opened_baseline.clone())?;
        if let Some((cols, rows)) = self.latest_resize {
            self.record(DisplayTrackEvent::Resize { cols, rows })?;
        }
        let repaint = bmux_terminal_grid::full_screen_repaint_bytes(self.replay_grid.grid());
        if !repaint.is_empty() {
            self.record(DisplayTrackEvent::FrameBytes { data: repaint })?;
        }
        Ok(())
    }

    fn record_cursor_snapshot(
        &mut self,
        cursor_state: Option<crate::runtime::attach::state::AttachCursorState>,
    ) -> Result<()> {
        let (x, y, visible) =
            cursor_state.map_or((0, 0, false), |state| (state.x, state.y, state.visible));
        self.record(DisplayTrackEvent::CursorSnapshot {
            x,
            y,
            visible,
            shape: display_cursor_shape_from_visual(self.cursor_replay_state.shape),
            blink_enabled: self.cursor_replay_state.blink_enabled,
        })
    }

    fn record(&mut self, event: DisplayTrackEvent) -> Result<()> {
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
                return Ok(());
            }
            self.last_image_count = count;
        }
        let mono_ns = u64::try_from(
            self.started_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)),
        )
        .unwrap_or(u64::MAX);
        let envelope = DisplayTrackEnvelope { mono_ns, event };
        write_frame(&mut self.writer, &envelope)
            .map_err(|e| anyhow::anyhow!("display track write_frame failed: {e}"))?;
        self.maybe_rotate(mono_ns)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .context("failed flushing display capture writer")
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
        self.flush()?;
        let old_path =
            display_track_segment_path(&self.recording_path, self.client_id, self.segment_index);
        self.closed_segments.push_back((old_path, end_ns));
        self.segment_index = self.segment_index.saturating_add(1);
        self.segment_start_ns = end_ns;
        let new_path =
            display_track_segment_path(&self.recording_path, self.client_id, self.segment_index);
        self.writer = BufWriter::new(open_display_track_file(&new_path)?);
        self.record_segment_baseline()?;
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
) {
    for command in receiver {
        match command {
            DisplayCaptureCommand::Event(event) => {
                if let Err(error) = writer.record(event) {
                    tracing::warn!(error = %error, "display capture write failed");
                }
            }
            DisplayCaptureCommand::CursorSnapshot(cursor_state) => {
                if let Err(error) = writer.record_cursor_snapshot(cursor_state) {
                    tracing::warn!(error = %error, "display capture cursor snapshot failed");
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
