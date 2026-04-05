//! Display track writer for playbook recordings.
//!
//! Uses the shared `DisplayTrackEvent` and `DisplayTrackEnvelope` types from
//! `bmux_ipc` for binary compatibility with the attach runtime's display tracks.
//!
//! Format: length-prefixed binary codec frames.

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use bmux_ipc::{DisplayActivityKind, DisplayCursorShape, DisplayTrackEnvelope, DisplayTrackEvent};
use uuid::Uuid;

/// Writer that produces the display track binary file alongside a recording.
pub struct PlaybookDisplayTrackWriter {
    started_at: Instant,
    writer: std::io::BufWriter<std::fs::File>,
    parser: vt100::Parser,
    cursor_shape: DisplayCursorShape,
    cursor_blink_enabled: bool,
}

impl PlaybookDisplayTrackWriter {
    /// Create a new display track writer in the given recording directory.
    ///
    /// Writes `owner-client-id.txt` and the initial `stream_opened` + `resize` events.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording directory cannot be created or files
    /// cannot be written.
    pub fn new(
        recording_dir: &Path,
        client_id: Uuid,
        recording_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        std::fs::create_dir_all(recording_dir).with_context(|| {
            format!("failed creating recording dir {}", recording_dir.display())
        })?;

        // Write owner-client-id.txt
        let owner_path = recording_dir.join("owner-client-id.txt");
        std::fs::write(&owner_path, client_id.to_string())
            .with_context(|| format!("failed writing {}", owner_path.display()))?;

        // Open display track file
        let track_path = recording_dir.join(format!("display-{client_id}.bin"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&track_path)
            .with_context(|| format!("failed opening {}", track_path.display()))?;

        let mut writer = Self {
            started_at: Instant::now(),
            writer: std::io::BufWriter::new(file),
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 4_096),
            cursor_shape: DisplayCursorShape::Block,
            cursor_blink_enabled: true,
        };

        // Write initial events
        writer.record(DisplayTrackEvent::StreamOpened {
            client_id,
            recording_id,
            // Headless playbook — no real terminal cell metrics
            cell_width_px: Some(8),
            cell_height_px: Some(16),
            window_width_px: Some(cols * 8),
            window_height_px: Some(rows * 16),
            terminal_profile: None,
        })?;

        writer.record(DisplayTrackEvent::Resize { cols, rows })?;

        Ok(writer)
    }

    /// Record terminal output bytes (a frame of data).
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the display track file fails.
    pub fn record_frame_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.update_cursor_style(data);
        self.parser.process(data);
        self.record(DisplayTrackEvent::FrameBytes {
            data: data.to_vec(),
        })?;
        self.record(DisplayTrackEvent::Activity {
            kind: DisplayActivityKind::Output,
        })?;
        self.record_cursor_snapshot()
    }

    /// Record a viewport resize.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the display track file fails.
    pub fn record_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
        self.record(DisplayTrackEvent::Resize { cols, rows })
    }

    /// Record an activity event.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the display track file fails.
    pub fn record_activity(&mut self, kind: DisplayActivityKind) -> Result<()> {
        self.record(DisplayTrackEvent::Activity { kind })
    }

    /// Record the stream closed event and flush.
    ///
    /// # Errors
    ///
    /// Returns an error if flushing the display track writer fails.
    pub fn finish(&mut self) -> Result<()> {
        self.record(DisplayTrackEvent::StreamClosed)?;
        self.writer
            .flush()
            .context("failed flushing display track writer")
    }

    #[allow(clippy::cast_possible_truncation)]
    fn record(&mut self, event: DisplayTrackEvent) -> Result<()> {
        let envelope = DisplayTrackEnvelope {
            mono_ns: self
                .started_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64, // safe: clamped to u64::MAX
            event,
        };
        bmux_ipc::write_frame(&mut self.writer, &envelope)
            .map_err(|e| anyhow::anyhow!("display track write_frame failed: {e}"))?;
        Ok(())
    }

    fn record_cursor_snapshot(&mut self) -> Result<()> {
        let (y, x) = self.parser.screen().cursor_position();
        self.record(DisplayTrackEvent::CursorSnapshot {
            x,
            y,
            visible: !self.parser.screen().hide_cursor(),
            shape: self.cursor_shape,
            blink_enabled: self.cursor_blink_enabled,
        })
    }

    fn update_cursor_style(&mut self, data: &[u8]) {
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
                    self.cursor_shape = DisplayCursorShape::Block;
                    self.cursor_blink_enabled = true;
                }
                2 => {
                    self.cursor_shape = DisplayCursorShape::Block;
                    self.cursor_blink_enabled = false;
                }
                3 => {
                    self.cursor_shape = DisplayCursorShape::Underline;
                    self.cursor_blink_enabled = true;
                }
                4 => {
                    self.cursor_shape = DisplayCursorShape::Underline;
                    self.cursor_blink_enabled = false;
                }
                5 => {
                    self.cursor_shape = DisplayCursorShape::Bar;
                    self.cursor_blink_enabled = true;
                }
                6 => {
                    self.cursor_shape = DisplayCursorShape::Bar;
                    self.cursor_blink_enabled = false;
                }
                _ => {}
            }
            index = cursor + 2;
        }
    }
}
