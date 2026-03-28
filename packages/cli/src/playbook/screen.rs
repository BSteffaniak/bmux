//! Screen inspector: parse terminal output and extract text content for assertions.
//!
//! On each `refresh()`, a fresh `vt100::Parser` is created per pane and fed the
//! snapshot chunk data. This avoids the re-feed problem: `attach_snapshot` returns
//! the most recent bytes from a ring buffer (not cursor-based), so appending to an
//! existing parser would double-process overlapping data and corrupt state.
//!
//! Pane dimensions are extracted from the `AttachScene` surface rects, which give
//! exact pixel-accurate (cell-accurate) sizes for each pane including after splits.

use anyhow::{Context, Result, bail};
use bmux_client::{AttachSnapshotState, BmuxClient};
use regex::Regex;
use uuid::Uuid;

use super::types::PaneCapture;

/// Maximum bytes to request per pane in a snapshot.
const SNAPSHOT_MAX_BYTES_PER_PANE: usize = 256 * 1024;

/// Parsed screen state for a single pane after one `refresh()` cycle.
struct ParsedPane {
    #[allow(dead_code)]
    pane_id: Uuid,
    pane_index: u32,
    focused: bool,
    screen_text: String,
    cursor_row: u16,
    cursor_col: u16,
}

/// Inspector that parses terminal output into text for assertions.
///
/// Stateless between `refresh()` calls — each refresh produces a complete,
/// self-contained view of every pane's screen content.
pub struct ScreenInspector {
    /// Parsed pane state from the most recent `refresh()` call.
    panes: Vec<ParsedPane>,
    viewport_rows: u16,
    viewport_cols: u16,
}

impl ScreenInspector {
    pub fn new(viewport_cols: u16, viewport_rows: u16) -> Self {
        Self {
            panes: Vec::new(),
            viewport_rows,
            viewport_cols,
        }
    }

    pub fn update_viewport(&mut self, cols: u16, rows: u16) {
        self.viewport_cols = cols;
        self.viewport_rows = rows;
        self.panes.clear();
    }

    /// Fetch a fresh snapshot from the server and parse all pane screens.
    ///
    /// This creates a **fresh** vt100 parser for each pane on every call,
    /// avoiding the re-feed problem with `read_recent` (sliding window) data.
    /// Pane dimensions come from the scene's surface rects when available.
    pub async fn refresh(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
    ) -> Result<AttachSnapshotState> {
        let snapshot = client
            .attach_snapshot(session_id, SNAPSHOT_MAX_BYTES_PER_PANE)
            .await
            .map_err(|e| anyhow::anyhow!("snapshot failed: {e}"))?;

        // Build pane dimension map from the scene surfaces.
        let pane_dims = build_pane_dimensions(&snapshot);

        // Parse each pane's chunk data with a fresh parser.
        let mut parsed = Vec::new();
        for pane_summary in &snapshot.panes {
            let chunk = snapshot
                .chunks
                .iter()
                .find(|c| c.pane_id == pane_summary.id);

            // Get pane dimensions: prefer scene rect, fall back to viewport estimate.
            let (cols, rows) = pane_dims
                .iter()
                .find(|(id, _, _)| *id == pane_summary.id)
                .map(|(_, w, h)| (*w, *h))
                .unwrap_or_else(|| {
                    (
                        self.viewport_cols.saturating_sub(2).max(1),
                        self.viewport_rows.saturating_sub(2).max(1),
                    )
                });

            let mut parser = vt100::Parser::new(rows, cols, 4096);

            if let Some(chunk) = chunk {
                parser.process(&chunk.data);
            }

            let screen = parser.screen();
            let text = screen_to_text(screen);
            let (cursor_row, cursor_col) = screen.cursor_position();

            parsed.push(ParsedPane {
                pane_id: pane_summary.id,
                pane_index: pane_summary.index,
                focused: pane_summary.focused,
                screen_text: text,
                cursor_row,
                cursor_col,
            });
        }

        self.panes = parsed;
        Ok(snapshot)
    }

    /// Get the full screen text of a specific pane (by index).
    pub fn pane_text(&self, pane_index: u32) -> Option<String> {
        self.panes
            .iter()
            .find(|p| p.pane_index == pane_index)
            .map(|p| p.screen_text.clone())
    }

    /// Get the full screen text of the focused pane.
    #[allow(dead_code)]
    pub fn focused_pane_text(&self) -> Option<String> {
        self.panes
            .iter()
            .find(|p| p.focused)
            .map(|p| p.screen_text.clone())
    }

    /// Get cursor position for a pane (by index). Returns (row, col).
    pub fn pane_cursor(&self, pane_index: u32) -> Option<(u16, u16)> {
        self.panes
            .iter()
            .find(|p| p.pane_index == pane_index)
            .map(|p| (p.cursor_row, p.cursor_col))
    }

    /// Capture the state of all panes for a snapshot.
    pub fn capture_all(&self) -> Vec<PaneCapture> {
        self.panes
            .iter()
            .map(|p| PaneCapture {
                index: p.pane_index,
                focused: p.focused,
                screen_text: p.screen_text.clone(),
                cursor_row: p.cursor_row,
                cursor_col: p.cursor_col,
            })
            .collect()
    }

    /// Capture all panes, returning `None` if no panes are available.
    /// Safe to call even when the inspector has not been refreshed.
    pub fn capture_all_safe(&self) -> Option<Vec<PaneCapture>> {
        if self.panes.is_empty() {
            None
        } else {
            Some(self.capture_all())
        }
    }

    /// Check if a pane's screen text contains a substring.
    pub fn pane_contains(&self, pane_index: u32, needle: &str) -> bool {
        self.pane_text(pane_index)
            .map_or(false, |text| text.contains(needle))
    }

    /// Check if a pane's screen text matches a regex.
    pub fn pane_matches(&self, pane_index: u32, pattern: &str) -> Result<bool> {
        let re = Regex::new(pattern).with_context(|| format!("invalid regex: {pattern}"))?;
        Ok(self.pane_matches_compiled(pane_index, &re))
    }

    /// Check if a pane's screen text matches a pre-compiled regex.
    /// Use this in hot loops (e.g. `wait-for` polling) to avoid recompiling.
    pub fn pane_matches_compiled(&self, pane_index: u32, re: &Regex) -> bool {
        self.pane_text(pane_index)
            .map_or(false, |text| re.is_match(&text))
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
}

/// Extract per-pane (cols, rows) from the scene's surface rects.
///
/// Each pane has an `AttachSurface` in the scene with `pane_id` set and a `rect`
/// giving the exact cell dimensions.
fn build_pane_dimensions(snapshot: &AttachSnapshotState) -> Vec<(Uuid, u16, u16)> {
    snapshot
        .scene
        .surfaces
        .iter()
        .filter_map(|surface| {
            let pane_id = surface.pane_id?;
            // Only consider visible pane surfaces
            if !surface.visible {
                return None;
            }
            let w = surface.rect.w.max(1);
            let h = surface.rect.h.max(1);
            Some((pane_id, w, h))
        })
        .collect()
}

/// Extract visible text from a vt100 screen, with trailing whitespace trimmed per line.
fn screen_to_text(screen: &vt100::Screen) -> String {
    let mut lines = Vec::new();
    let (rows, cols) = screen.size();
    for row in 0..rows {
        let row_text = screen.contents_between(row, 0, row, cols);
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

    #[test]
    fn fresh_parser_avoids_double_processing() {
        // Simulates what would happen if we re-fed the same data twice
        // to an accumulated parser vs. using a fresh one each time.
        let data = b"hello\r\nworld\r\n";

        // Accumulated parser (old buggy approach): process same data twice
        let mut accumulated = vt100::Parser::new(24, 80, 100);
        accumulated.process(data);
        accumulated.process(data); // re-feed!
        let accumulated_text = screen_to_text(accumulated.screen());

        // Fresh parser (correct approach): process once
        let mut fresh = vt100::Parser::new(24, 80, 100);
        fresh.process(data);
        let fresh_text = screen_to_text(fresh.screen());

        // The fresh parser should show "hello\nworld"
        assert_eq!(fresh_text, "hello\nworld");
        // The accumulated parser shows doubled output — this is the bug we fixed
        assert_ne!(accumulated_text, fresh_text);
    }
}
