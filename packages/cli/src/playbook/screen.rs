//! Screen inspector: maintain persistent terminal parsers per pane and expose
//! text/cursor state for playbook assertions.
//!
//! Unlike snapshot-tail reparsing, this keeps parser state alive and feeds
//! incremental pane output chunks in stream order. This matches the incremental
//! parser model used by mature multiplexers.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use bmux_client::{AttachLayoutState, AttachSnapshotState, BmuxClient};
use regex::Regex;
use serde::Serialize;
use uuid::Uuid;

use super::types::PaneCapture;

/// Maximum bytes to request per pane in a snapshot.
const SNAPSHOT_MAX_BYTES_PER_PANE: usize = 256 * 1024;
/// Maximum bytes to request per pane from incremental pane-output batches.
const OUTPUT_BATCH_MAX_BYTES: usize = 256 * 1024;

/// Parsed screen state for a single pane after one synchronization cycle.
struct ParsedPane {
    _pane_id: Uuid,
    pane_index: u32,
    focused: bool,
    screen_text: String,
    cursor_row: u16,
    cursor_col: u16,
}

struct PaneStreamState {
    pane_id: Uuid,
    pane_index: u32,
    focused: bool,
    rows: u16,
    cols: u16,
    parser: vt100::Parser,
    /// Expected start offset of the next incremental chunk for this pane.
    /// None means no continuity baseline has been established yet.
    expected_stream_start: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct OutputDrainResult {
    /// True when bytes were processed or a deterministic resync happened.
    pub had_activity: bool,
    /// Bytes from the currently focused pane in this drain call.
    pub focused_output: Vec<u8>,
    /// Server-side hint that more pane output remains to drain.
    pub output_still_pending: bool,
    /// True when at least one pane reports DEC 2026 synchronized-update active.
    pub any_sync_update_active: bool,
}

/// Inspector that parses terminal output into text for assertions.
pub struct ScreenInspector {
    /// Parsed pane state from the most recent synchronization cycle.
    panes: Vec<ParsedPane>,
    /// Persistent parser state keyed by pane id.
    pane_states: BTreeMap<Uuid, PaneStreamState>,
    viewport_rows: u16,
    viewport_cols: u16,
    session_id: Option<Uuid>,
    needs_bootstrap: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenDeltaFormat {
    LineOps,
    UnifiedDiff,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CursorPosition {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CursorDeltaEvent {
    pub pane_index: u32,
    pub from: CursorPosition,
    pub to: CursorPosition,
    pub distance: u16,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ScreenLineOp {
    SetLine { row: u16, text: String },
    ClearLine { row: u16 },
    Cursor { row: u16, col: u16 },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScreenDeltaEvent {
    pub pane_index: u32,
    pub format: ScreenDeltaFormat,
    pub base_hash: String,
    pub new_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ops: Option<Vec<ScreenLineOp>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PaneDeltaResult {
    pub pane: PaneCapture,
    pub cursor_delta: Option<CursorDeltaEvent>,
    pub screen_delta: Option<ScreenDeltaEvent>,
}

impl ScreenInspector {
    pub fn new(viewport_cols: u16, viewport_rows: u16) -> Self {
        Self {
            panes: Vec::new(),
            pane_states: BTreeMap::new(),
            viewport_rows,
            viewport_cols,
            session_id: None,
            needs_bootstrap: true,
        }
    }

    pub fn update_viewport(&mut self, cols: u16, rows: u16) {
        self.viewport_cols = cols;
        self.viewport_rows = rows;
        self.panes.clear();
        self.pane_states.clear();
        self.session_id = None;
        self.needs_bootstrap = true;
    }

    /// Synchronize layout/output and update parsed pane state.
    pub async fn refresh(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
    ) -> Result<AttachSnapshotState> {
        let (layout, _) = self
            .sync_and_drain(client, session_id, OUTPUT_BATCH_MAX_BYTES)
            .await?;
        Ok(snapshot_from_layout(layout))
    }

    /// Drain one incremental pane-output batch and update parser state.
    pub async fn drain_incremental_output(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> Result<OutputDrainResult> {
        let (_, drain) = self
            .sync_and_drain(client, session_id, max_bytes_per_pane.max(1))
            .await?;
        Ok(drain)
    }

    async fn sync_and_drain(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> Result<(AttachLayoutState, OutputDrainResult)> {
        self.reset_for_session(session_id);

        let mut layout = client
            .attach_layout(session_id)
            .await
            .map_err(|e| anyhow::anyhow!("layout failed: {e}"))?;

        let pane_set_changed = self.apply_layout_state(&layout);
        if pane_set_changed {
            self.needs_bootstrap = true;
        }

        if self.needs_bootstrap {
            let snapshot = self.bootstrap_from_snapshot(client, session_id).await?;
            layout = layout_from_snapshot(&snapshot);
            let _ = self.apply_layout_state(&layout);
        }

        let drain = self
            .drain_output_batch(client, session_id, &layout, max_bytes_per_pane)
            .await?;

        self.rebuild_parsed_panes();
        Ok((layout, drain))
    }

    fn reset_for_session(&mut self, session_id: Uuid) {
        if self.session_id == Some(session_id) {
            return;
        }
        self.session_id = Some(session_id);
        self.panes.clear();
        self.pane_states.clear();
        self.needs_bootstrap = true;
    }

    fn apply_layout_state(&mut self, layout: &AttachLayoutState) -> bool {
        let pane_ids = layout
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<BTreeSet<_>>();
        let existing_ids = self.pane_states.keys().copied().collect::<BTreeSet<_>>();
        let pane_set_changed = pane_ids != existing_ids;

        self.pane_states
            .retain(|pane_id, _| pane_ids.contains(pane_id));

        let pane_dims = build_pane_dimensions_from_scene(&layout.scene);
        for pane in &layout.panes {
            let (cols, rows) = pane_dims
                .get(&pane.id)
                .copied()
                .unwrap_or_else(|| self.default_pane_dimensions());

            let state = self
                .pane_states
                .entry(pane.id)
                .or_insert_with(|| PaneStreamState {
                    pane_id: pane.id,
                    pane_index: pane.index,
                    focused: pane.focused,
                    rows,
                    cols,
                    parser: vt100::Parser::new(rows, cols, 4_096),
                    expected_stream_start: None,
                });

            state.pane_index = pane.index;
            state.focused = pane.focused;

            if state.rows != rows || state.cols != cols {
                state.rows = rows;
                state.cols = cols;
                state.parser.screen_mut().set_size(rows, cols);
            }
        }

        pane_set_changed
    }

    async fn bootstrap_from_snapshot(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
    ) -> Result<AttachSnapshotState> {
        let snapshot = client
            .attach_snapshot(session_id, SNAPSHOT_MAX_BYTES_PER_PANE)
            .await
            .map_err(|e| anyhow::anyhow!("snapshot failed: {e}"))?;

        let pane_dims = build_pane_dimensions_from_scene(&snapshot.scene);

        let mut next_states = BTreeMap::new();
        for pane in &snapshot.panes {
            let (cols, rows) = pane_dims
                .get(&pane.id)
                .copied()
                .unwrap_or_else(|| self.default_pane_dimensions());

            let mut parser = vt100::Parser::new(rows, cols, 4_096);
            if let Some(chunk) = snapshot
                .chunks
                .iter()
                .find(|chunk| chunk.pane_id == pane.id)
            {
                parser.process(&chunk.data);
            }

            next_states.insert(
                pane.id,
                PaneStreamState {
                    pane_id: pane.id,
                    pane_index: pane.index,
                    focused: pane.focused,
                    rows,
                    cols,
                    parser,
                    // Snapshot data is a recent tail, not offset-addressed stream
                    // data, so continuity baseline starts with the first batch.
                    expected_stream_start: None,
                },
            );
        }

        self.pane_states = next_states;
        self.needs_bootstrap = false;
        self.rebuild_parsed_panes();

        Ok(snapshot)
    }

    async fn drain_output_batch(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
        layout: &AttachLayoutState,
        max_bytes_per_pane: usize,
    ) -> Result<OutputDrainResult> {
        let pane_ids = layout.panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        if pane_ids.is_empty() {
            return Ok(OutputDrainResult::default());
        }

        let batch = client
            .attach_pane_output_batch(session_id, pane_ids, max_bytes_per_pane.max(1))
            .await
            .map_err(|e| anyhow::anyhow!("pane output batch failed: {e}"))?;

        self.apply_batch(layout, batch, client, session_id).await
    }

    async fn apply_batch(
        &mut self,
        layout: &AttachLayoutState,
        batch: bmux_client::PaneOutputBatchResult,
        client: &mut BmuxClient,
        session_id: Uuid,
    ) -> Result<OutputDrainResult> {
        let mut result = OutputDrainResult {
            had_activity: false,
            focused_output: Vec::new(),
            output_still_pending: batch.output_still_pending,
            any_sync_update_active: false,
        };

        let mut needs_resync = false;
        for chunk in batch.chunks {
            if chunk.pane_id == layout.focused_pane_id && !chunk.data.is_empty() {
                result.focused_output.extend_from_slice(&chunk.data);
            }

            result.any_sync_update_active |= chunk.sync_update_active;

            let Some(state) = self.pane_states.get_mut(&chunk.pane_id) else {
                needs_resync = true;
                continue;
            };

            if chunk.stream_end < chunk.stream_start {
                needs_resync = true;
                continue;
            }

            if chunk.stream_gap {
                needs_resync = true;
                continue;
            }

            if let Some(expected) = state.expected_stream_start {
                if chunk.stream_start != expected {
                    needs_resync = true;
                    continue;
                }
            }

            if !chunk.data.is_empty() {
                state.parser.process(&chunk.data);
                result.had_activity = true;
            }

            state.expected_stream_start = Some(chunk.stream_end);
        }

        if needs_resync {
            let _ = self.bootstrap_from_snapshot(client, session_id).await?;
            result.had_activity = true;
        }

        Ok(result)
    }

    fn rebuild_parsed_panes(&mut self) {
        let mut panes = self
            .pane_states
            .values()
            .map(|state| {
                let screen = state.parser.screen();
                let (cursor_row, cursor_col) = screen.cursor_position();
                ParsedPane {
                    _pane_id: state.pane_id,
                    pane_index: state.pane_index,
                    focused: state.focused,
                    screen_text: screen_to_text(screen),
                    cursor_row,
                    cursor_col,
                }
            })
            .collect::<Vec<_>>();
        panes.sort_by_key(|pane| pane.pane_index);
        self.panes = panes;
    }

    fn default_pane_dimensions(&self) -> (u16, u16) {
        (
            self.viewport_cols.saturating_sub(2).max(1),
            self.viewport_rows.saturating_sub(2).max(1),
        )
    }

    /// Get the full screen text of a specific pane (by index).
    pub fn pane_text(&self, pane_index: u32) -> Option<String> {
        self.panes
            .iter()
            .find(|p| p.pane_index == pane_index)
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

    /// Build cursor and screen deltas against a previous pane cache.
    pub fn build_deltas(
        &self,
        previous: &std::collections::HashMap<u32, PaneCapture>,
        format: ScreenDeltaFormat,
    ) -> Vec<PaneDeltaResult> {
        self.capture_all()
            .into_iter()
            .map(|pane| {
                let prior = previous.get(&pane.index);
                let cursor_delta = build_cursor_delta(prior, &pane);
                let screen_delta = build_screen_delta(prior, &pane, format);
                PaneDeltaResult {
                    pane,
                    cursor_delta,
                    screen_delta,
                }
            })
            .collect()
    }

    /// Check if a pane's screen text contains a substring.
    pub fn pane_contains(&self, pane_index: u32, needle: &str) -> bool {
        self.pane_text(pane_index)
            .is_some_and(|text| text.contains(needle))
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
            .is_some_and(|text| re.is_match(&text))
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

fn snapshot_from_layout(layout: AttachLayoutState) -> AttachSnapshotState {
    AttachSnapshotState {
        context_id: layout.context_id,
        session_id: layout.session_id,
        focused_pane_id: layout.focused_pane_id,
        panes: layout.panes,
        layout_root: layout.layout_root,
        scene: layout.scene,
        chunks: Vec::new(),
        pane_mouse_protocols: Vec::new(),
        zoomed: layout.zoomed,
    }
}

fn layout_from_snapshot(snapshot: &AttachSnapshotState) -> AttachLayoutState {
    AttachLayoutState {
        context_id: snapshot.context_id,
        session_id: snapshot.session_id,
        focused_pane_id: snapshot.focused_pane_id,
        panes: snapshot.panes.clone(),
        layout_root: snapshot.layout_root.clone(),
        scene: snapshot.scene.clone(),
        zoomed: snapshot.zoomed,
    }
}

fn build_cursor_delta(
    previous: Option<&PaneCapture>,
    current: &PaneCapture,
) -> Option<CursorDeltaEvent> {
    let prior = previous?;
    if prior.cursor_row == current.cursor_row && prior.cursor_col == current.cursor_col {
        return None;
    }
    let row_delta = current.cursor_row.abs_diff(prior.cursor_row);
    let col_delta = current.cursor_col.abs_diff(prior.cursor_col);
    Some(CursorDeltaEvent {
        pane_index: current.index,
        from: CursorPosition {
            row: prior.cursor_row,
            col: prior.cursor_col,
        },
        to: CursorPosition {
            row: current.cursor_row,
            col: current.cursor_col,
        },
        distance: row_delta.saturating_add(col_delta),
    })
}

fn build_screen_delta(
    previous: Option<&PaneCapture>,
    current: &PaneCapture,
    format: ScreenDeltaFormat,
) -> Option<ScreenDeltaEvent> {
    let previous_text = previous.map_or("", |p| p.screen_text.as_str());
    if previous.is_some_and(|p| p.screen_text == current.screen_text) {
        return None;
    }

    let base_hash = text_hash(previous_text);
    let new_hash = text_hash(&current.screen_text);
    match format {
        ScreenDeltaFormat::LineOps => {
            let mut ops = line_ops_delta(previous_text, &current.screen_text);
            ops.push(ScreenLineOp::Cursor {
                row: current.cursor_row,
                col: current.cursor_col,
            });
            Some(ScreenDeltaEvent {
                pane_index: current.index,
                format,
                base_hash,
                new_hash,
                ops: Some(ops),
                diff: None,
            })
        }
        ScreenDeltaFormat::UnifiedDiff => {
            let diff = unified_diff(previous_text, &current.screen_text)?;
            Some(ScreenDeltaEvent {
                pane_index: current.index,
                format,
                base_hash,
                new_hash,
                ops: None,
                diff: Some(diff),
            })
        }
    }
}

fn line_ops_delta(previous_text: &str, current_text: &str) -> Vec<ScreenLineOp> {
    let previous_lines = previous_text.lines().collect::<Vec<_>>();
    let current_lines = current_text.lines().collect::<Vec<_>>();
    let max_len = previous_lines.len().max(current_lines.len());
    let mut ops = Vec::new();
    for row in 0..max_len {
        match (previous_lines.get(row), current_lines.get(row)) {
            (Some(prev), Some(curr)) if prev != curr => ops.push(ScreenLineOp::SetLine {
                row: row as u16,
                text: (*curr).to_string(),
            }),
            (None, Some(curr)) => ops.push(ScreenLineOp::SetLine {
                row: row as u16,
                text: (*curr).to_string(),
            }),
            (Some(_), None) => ops.push(ScreenLineOp::ClearLine { row: row as u16 }),
            _ => {}
        }
    }
    ops
}

fn unified_diff(previous_text: &str, current_text: &str) -> Option<String> {
    let previous_lines = previous_text.lines().collect::<Vec<_>>();
    let current_lines = current_text.lines().collect::<Vec<_>>();
    let max_len = previous_lines.len().max(current_lines.len());
    let mut output = String::new();
    for row in 0..max_len {
        let prev = previous_lines.get(row).copied().unwrap_or("");
        let curr = current_lines.get(row).copied().unwrap_or("");
        if prev == curr {
            continue;
        }
        output.push_str(&format!("@@ -{},1 +{},1 @@\n", row + 1, row + 1));
        output.push_str(&format!("-{prev}\n"));
        output.push_str(&format!("+{curr}\n"));
    }
    if output.is_empty() {
        None
    } else {
        Some(output)
    }
}

fn text_hash(text: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Extract per-pane (cols, rows) from scene surface rects.
fn build_pane_dimensions_from_scene(scene: &bmux_ipc::AttachScene) -> BTreeMap<Uuid, (u16, u16)> {
    scene
        .surfaces
        .iter()
        .filter_map(|surface| {
            let pane_id = surface.pane_id?;
            if !surface.visible {
                return None;
            }
            let cols = surface.rect.w.max(1);
            let rows = surface.rect.h.max(1);
            Some((pane_id, (cols, rows)))
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
    while lines.last().is_some_and(String::is_empty) {
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
    fn incremental_parser_handles_split_alt_exit_sequence() {
        let mut parser = vt100::Parser::new(30, 120, 4_096);

        parser.process(b"\x1b[12;34H");
        parser.process(b"\x1b[?1049h\x1b[2J\x1b[HSEQ_TUI");
        parser.process(b"\x1b[?10");
        parser.process(b"49l");

        let (row, col) = parser.screen().cursor_position();
        assert_eq!((row, col), (11, 33));
    }

    #[test]
    fn line_ops_delta_reports_changed_rows() {
        let ops = line_ops_delta("a\nb", "a\nc");
        assert!(matches!(
            ops.as_slice(),
            [ScreenLineOp::SetLine { row: 1, text }] if text == "c"
        ));
    }

    #[test]
    fn unified_diff_reports_changed_rows() {
        let diff = unified_diff("hello", "hullo").expect("diff should exist");
        assert!(diff.contains("@@ -1,1 +1,1 @@"));
        assert!(diff.contains("-hello"));
        assert!(diff.contains("+hullo"));
    }
}
