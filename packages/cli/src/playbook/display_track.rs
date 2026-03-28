//! Display track writer for playbook recordings.
//!
//! Produces the same JSONL format as the attach runtime's `DisplayCaptureWriter`,
//! enabling GIF export from playbook recordings.
//!
//! Format: one JSON object per line (NDJSON), each containing:
//! - `mono_ns`: monotonic nanosecond timestamp from session start
//! - `event`: one of `stream_opened`, `resize`, `frame_bytes`, `stream_closed`

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

/// Display track event — matches the format used by `recording/mod.rs`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum DisplayTrackEvent {
    StreamOpened {
        client_id: Uuid,
        recording_id: Uuid,
        cell_width_px: Option<u16>,
        cell_height_px: Option<u16>,
        window_width_px: Option<u16>,
        window_height_px: Option<u16>,
        terminal_profile: Option<()>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    FrameBytes {
        data: Vec<u8>,
    },
    StreamClosed,
}

/// Display track envelope — wraps an event with a monotonic timestamp.
#[derive(Debug, Clone, Serialize)]
struct DisplayTrackEnvelope {
    mono_ns: u64,
    event: DisplayTrackEvent,
}

/// Writer that produces the display track JSONL file alongside a recording.
pub struct PlaybookDisplayTrackWriter {
    started_at: Instant,
    writer: std::io::BufWriter<std::fs::File>,
}

impl PlaybookDisplayTrackWriter {
    /// Create a new display track writer in the given recording directory.
    ///
    /// Writes `owner-client-id.txt` and the initial `stream_opened` + `resize` events.
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
        let track_path = recording_dir.join(format!("display-{client_id}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&track_path)
            .with_context(|| format!("failed opening {}", track_path.display()))?;

        let mut writer = Self {
            started_at: Instant::now(),
            writer: std::io::BufWriter::new(file),
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
    pub fn record_frame_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.record(DisplayTrackEvent::FrameBytes {
            data: data.to_vec(),
        })
    }

    /// Record a viewport resize.
    pub fn record_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.record(DisplayTrackEvent::Resize { cols, rows })
    }

    /// Record the stream closed event and flush.
    pub fn finish(&mut self) -> Result<()> {
        self.record(DisplayTrackEvent::StreamClosed)?;
        self.writer
            .flush()
            .context("failed flushing display track writer")
    }

    fn record(&mut self, event: DisplayTrackEvent) -> Result<()> {
        let envelope = DisplayTrackEnvelope {
            mono_ns: self
                .started_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
            event,
        };
        serde_json::to_writer(&mut self.writer, &envelope)?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }
}
