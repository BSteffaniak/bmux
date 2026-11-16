//! Opt-in end-to-end latency log for `bmux-bench replay` (see
//! `docs/development/pane-output-performance.md`).
//!
//! With `BMUX_REPLAY_LOG` naming a file, the attach client notes each replay marker in the pane
//! output it applies, and when the host frame that shows it is handed to the host terminal,
//! appends `C frame written_ns applied_ns render_started_ns presented_ns` to the file
//! (nanoseconds since the Unix epoch). Without
//! it, nothing is scanned or written.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct ReplayLog {
    file: Option<std::fs::File>,
    /// Markers applied but not yet shown: frame, when the application wrote it, when applied.
    pending: Vec<(u64, u128, u128)>,
    /// When the frame being rendered started.
    render_started: u128,
}

impl ReplayLog {
    /// Open the log named by `BMUX_REPLAY_LOG`, if any.
    #[must_use]
    pub fn from_env() -> Self {
        let file = std::env::var_os("BMUX_REPLAY_LOG").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        Self {
            file,
            pending: Vec::new(),
            render_started: 0,
        }
    }

    /// Note the markers in pane output just applied.
    pub fn applied(&mut self, bytes: &[u8]) {
        if self.file.is_some() {
            let applied = now_ns();
            self.pending.extend(
                bmux_bench::corpus::markers(bytes)
                    .into_iter()
                    .map(|(frame, written)| (frame, written, applied)),
            );
        }
    }

    /// A frame showing everything applied so far starts rendering.
    pub fn rendering(&mut self) {
        if self.file.is_some() {
            self.render_started = now_ns();
        }
    }

    /// The host frame showing everything applied so far was handed to the host terminal.
    pub fn presented(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.pending.is_empty() {
            return;
        }
        let now = now_ns();
        let mut lines = Vec::new();
        for (frame, written, applied) in self.pending.drain(..) {
            let started = self.render_started.max(applied);
            let _ = writeln!(lines, "C {frame} {written} {applied} {started} {now}");
        }
        // Best effort: a measurement aid must never disturb attach.
        let _ = file.write_all(&lines);
    }
}

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}
