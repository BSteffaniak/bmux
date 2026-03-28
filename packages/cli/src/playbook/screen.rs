//! Screen inspector: parse terminal output and extract text content for assertions.

use anyhow::{Context, Result, bail};
use bmux_client::{AttachSnapshotState, BmuxClient};
use regex::Regex;
use uuid::Uuid;

use super::types::PaneCapture;

/// Maximum bytes to request per pane in a snapshot.
const SNAPSHOT_MAX_BYTES_PER_PANE: usize = 256 * 1024;

/// Inspector that maintains per-pane vt100 parsers and provides text extraction.
pub struct ScreenInspector {
    /// Per-pane parser state, keyed by pane UUID.
    parsers: Vec<(Uuid, u32, vt100::Parser)>,
    viewport_rows: u16,
    viewport_cols: u16,
}

impl ScreenInspector {
    pub fn new(viewport_cols: u16, viewport_rows: u16) -> Self {
        Self {
            parsers: Vec::new(),
            viewport_rows,
            viewport_cols,
        }
    }

    pub fn update_viewport(&mut self, cols: u16, rows: u16) {
        self.viewport_cols = cols;
        self.viewport_rows = rows;
        // Reset parsers on resize since dimensions changed
        self.parsers.clear();
    }

    /// Fetch a fresh snapshot from the server and update internal parser state.
    pub async fn refresh(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
    ) -> Result<AttachSnapshotState> {
        let snapshot = client
            .attach_snapshot(session_id, SNAPSHOT_MAX_BYTES_PER_PANE)
            .await
            .map_err(|e| anyhow::anyhow!("snapshot failed: {e}"))?;

        // Update parsers for each pane
        for chunk in &snapshot.chunks {
            if let Some(pane_summary) = snapshot.panes.iter().find(|p| p.id == chunk.pane_id) {
                let parser = self.get_or_create_parser(chunk.pane_id, pane_summary.index);
                parser.process(&chunk.data);
            }
        }

        Ok(snapshot)
    }

    /// Get the full screen text of a specific pane (by index).
    /// Returns the text with trailing whitespace trimmed per line.
    pub fn pane_text(&self, pane_index: u32) -> Option<String> {
        self.parsers
            .iter()
            .find(|(_, idx, _)| *idx == pane_index)
            .map(|(_, _, parser)| screen_to_text(parser.screen()))
    }

    /// Get the full screen text of the focused pane.
    #[allow(dead_code)]
    pub fn focused_pane_text(&self, snapshot: &AttachSnapshotState) -> Option<String> {
        let focused = snapshot.panes.iter().find(|p| p.focused)?;
        self.pane_text(focused.index)
    }

    /// Get cursor position for a pane (by index). Returns (row, col).
    pub fn pane_cursor(&self, pane_index: u32) -> Option<(u16, u16)> {
        self.parsers
            .iter()
            .find(|(_, idx, _)| *idx == pane_index)
            .map(|(_, _, parser)| {
                let screen = parser.screen();
                (screen.cursor_position().0, screen.cursor_position().1)
            })
    }

    /// Capture the state of all panes for a snapshot.
    pub fn capture_all(&self, snapshot: &AttachSnapshotState) -> Vec<PaneCapture> {
        snapshot
            .panes
            .iter()
            .map(|pane| {
                let (screen_text, cursor_row, cursor_col) = self
                    .parsers
                    .iter()
                    .find(|(_, idx, _)| *idx == pane.index)
                    .map(|(_, _, parser)| {
                        let screen = parser.screen();
                        let text = screen_to_text(screen);
                        let (row, col) = screen.cursor_position();
                        (text, row, col)
                    })
                    .unwrap_or_default();

                PaneCapture {
                    index: pane.index,
                    focused: pane.focused,
                    screen_text,
                    cursor_row,
                    cursor_col,
                }
            })
            .collect()
    }

    /// Check if a pane's screen text contains a substring.
    pub fn pane_contains(&self, pane_index: u32, needle: &str) -> bool {
        self.pane_text(pane_index)
            .map_or(false, |text| text.contains(needle))
    }

    /// Check if a pane's screen text matches a regex.
    pub fn pane_matches(&self, pane_index: u32, pattern: &str) -> Result<bool> {
        let re = Regex::new(pattern).with_context(|| format!("invalid regex: {pattern}"))?;
        Ok(self
            .pane_text(pane_index)
            .map_or(false, |text| re.is_match(&text)))
    }

    /// Resolve which pane index to inspect. If `pane` is `None`, uses the focused pane.
    pub fn resolve_pane_index(
        &self,
        pane: Option<u32>,
        snapshot: &AttachSnapshotState,
    ) -> Result<u32> {
        match pane {
            Some(idx) => {
                if snapshot.panes.iter().any(|p| p.index == idx) {
                    Ok(idx)
                } else {
                    bail!("pane index {idx} not found")
                }
            }
            None => snapshot
                .panes
                .iter()
                .find(|p| p.focused)
                .map(|p| p.index)
                .context("no focused pane"),
        }
    }

    fn get_or_create_parser(&mut self, pane_id: Uuid, pane_index: u32) -> &mut vt100::Parser {
        let pos = self.parsers.iter().position(|(id, _, _)| *id == pane_id);
        match pos {
            Some(i) => &mut self.parsers[i].2,
            None => {
                // Estimate pane size from viewport — for a single pane it gets
                // the full viewport minus borders; for splits it's smaller.
                // Using full viewport as an approximation since we don't have
                // exact per-pane dimensions here.
                let rows = self.viewport_rows.saturating_sub(2).max(1);
                let cols = self.viewport_cols.saturating_sub(2).max(1);
                self.parsers
                    .push((pane_id, pane_index, vt100::Parser::new(rows, cols, 4096)));
                &mut self.parsers.last_mut().unwrap().2
            }
        }
    }
}

/// Extract visible text from a vt100 screen, with trailing whitespace trimmed per line.
fn screen_to_text(screen: &vt100::Screen) -> String {
    let mut lines = Vec::new();
    let (rows, _cols) = screen.size();
    for row in 0..rows {
        let row_text = screen.contents_between(row, 0, row, screen.size().1);
        lines.push(row_text.trim_end().to_string());
    }

    // Trim trailing empty lines
    while lines.last().map_or(false, |l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_to_text_basic() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"hello world\r\nsecond line");
        let text = screen_to_text(parser.screen());
        assert!(text.contains("hello world"));
        assert!(text.contains("second line"));
    }

    #[test]
    fn screen_to_text_trims_trailing_empty_lines() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(b"line one\r\nline two");
        let text = screen_to_text(parser.screen());
        assert!(!text.ends_with('\n'));
        let line_count = text.lines().count();
        assert_eq!(line_count, 2);
    }
}
