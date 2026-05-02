//! Screen inspector: maintain persistent terminal parsers per pane and expose
//! text/cursor state for playbook assertions.
//!
//! Unlike snapshot-tail reparsing, this keeps parser state alive and feeds
//! incremental pane output chunks in stream order. This matches the incremental
//! parser model used by mature multiplexers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use bmux_attach_pipeline::PaneRenderBuffer;
use bmux_client::{AttachLayoutState, AttachSnapshotState, BmuxClient};
use regex::Regex;
use serde::Serialize;
use uuid::Uuid;

use super::types::PaneCapture;
use crate::pane_runtime_client::{BmuxPaneRuntimeClientExt, attach_pane_grid_snapshot_state};

/// Maximum bytes to request per pane in a snapshot.
const SNAPSHOT_MAX_BYTES_PER_PANE: usize = 256 * 1024;
/// Maximum bytes to request per pane from incremental pane-output batches.
const OUTPUT_BATCH_MAX_BYTES: usize = 256 * 1024;
/// Maximum structured rows to request per pane for playbook assertions.
const GRID_SNAPSHOT_MAX_ROWS_PER_PANE: usize = 100_000;

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
    terminal_grid: Option<bmux_terminal_grid::TerminalGridStream>,
    /// Expected start offset of the next incremental chunk for this pane.
    /// None means no continuity baseline has been established yet.
    expected_stream_start: Option<u64>,
    sync_update_active: bool,
}

#[derive(Debug, Clone)]
pub struct PaneOutputChunk {
    pub pane_index: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct OutputDrainResult {
    /// True when bytes were processed or a deterministic resync happened.
    pub had_activity: bool,
    /// Bytes from the currently focused pane in this drain call.
    pub focused_output: Vec<u8>,
    /// Raw incremental output bytes grouped by pane index.
    pub pane_outputs: Vec<PaneOutputChunk>,
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
    #[must_use]
    pub const fn new(viewport_cols: u16, viewport_rows: u16) -> Self {
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

    #[must_use]
    pub const fn viewport_size(&self) -> (u16, u16) {
        (self.viewport_cols, self.viewport_rows)
    }

    /// Synchronize layout/output and update parsed pane state.
    /// # Errors
    ///
    /// Returns an error if the server request fails.
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
    /// # Errors
    ///
    /// Returns an error if reading pane output fails.
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

        self.hydrate_structured_grid_snapshots(client, session_id, &layout)
            .await;
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
                    terminal_grid: None,
                    expected_stream_start: None,
                    sync_update_active: false,
                });

            state.pane_index = pane.index;
            state.focused = pane.focused;

            if state.rows != rows || state.cols != cols {
                state.rows = rows;
                state.cols = cols;
                state.parser.screen_mut().set_size(rows, cols);
                if let Some(grid) = state.terminal_grid.as_mut() {
                    let _ = grid.resize_delta(cols.max(1), rows.max(1));
                }
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
                    terminal_grid: None,
                    // Snapshot data is a recent tail, not offset-addressed stream
                    // data, so continuity baseline starts with the first batch.
                    expected_stream_start: None,
                    sync_update_active: snapshot
                        .chunks
                        .iter()
                        .find(|chunk| chunk.pane_id == pane.id)
                        .is_some_and(|chunk| chunk.sync_update_active),
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
            pane_outputs: Vec::new(),
            output_still_pending: batch.output_still_pending,
            any_sync_update_active: false,
        };

        let mut needs_resync = false;
        for chunk in batch.chunks {
            let bmux_attach_layout_protocol::AttachPaneChunk {
                pane_id,
                data,
                stream_start,
                stream_end,
                stream_gap,
                sync_update_active,
            } = chunk;

            result.any_sync_update_active |= sync_update_active;

            let Some(state) = self.pane_states.get_mut(&pane_id) else {
                needs_resync = true;
                continue;
            };

            if stream_end < stream_start {
                needs_resync = true;
                continue;
            }

            if stream_gap {
                needs_resync = true;
                continue;
            }

            if let Some(expected) = state.expected_stream_start
                && stream_start != expected
            {
                needs_resync = true;
                continue;
            }

            if pane_id == layout.focused_pane_id && !data.is_empty() {
                result.focused_output.extend_from_slice(&data);
            }

            if !data.is_empty() {
                state.parser.process(&data);
                result.pane_outputs.push(PaneOutputChunk {
                    pane_index: state.pane_index,
                    data,
                });
                result.had_activity = true;
            }

            state.sync_update_active = sync_update_active;
            state.expected_stream_start = Some(stream_end);
        }

        if needs_resync {
            let _ = self.bootstrap_from_snapshot(client, session_id).await?;
            result.had_activity = true;
        }

        Ok(result)
    }

    async fn hydrate_structured_grid_snapshots(
        &mut self,
        client: &mut BmuxClient,
        session_id: Uuid,
        layout: &AttachLayoutState,
    ) {
        let pane_ids = layout.panes.iter().map(|pane| pane.id).collect::<Vec<_>>();
        if pane_ids.is_empty() {
            return;
        }

        let snapshots = match attach_pane_grid_snapshot_state(
            client,
            session_id,
            pane_ids,
            GRID_SNAPSHOT_MAX_ROWS_PER_PANE,
        )
        .await
        {
            Ok(snapshots) => snapshots,
            Err(error) => {
                tracing::debug!(%error, "structured pane grid snapshot unavailable for playbook inspector");
                return;
            }
        };

        for snapshot in snapshots {
            let decoded = match serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(
                &snapshot.encoded,
            ) {
                Ok(decoded) => decoded,
                Err(error) => {
                    tracing::warn!(pane_id = %snapshot.pane_id, %error, "failed decoding playbook structured pane grid snapshot");
                    continue;
                }
            };
            let stream = match bmux_terminal_grid::TerminalGridStream::from_snapshot(
                &decoded,
                bmux_terminal_grid::GridLimits::default(),
            ) {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(pane_id = %snapshot.pane_id, %error, "failed hydrating playbook structured pane grid snapshot");
                    continue;
                }
            };
            if let Some(state) = self.pane_states.get_mut(&snapshot.pane_id) {
                state.terminal_grid = Some(stream);
                state.expected_stream_start = Some(snapshot.stream_end);
            }
        }
    }

    fn rebuild_parsed_panes(&mut self) {
        let mut panes = self
            .pane_states
            .values()
            .map(|state| {
                state.terminal_grid.as_ref().map_or_else(
                    || {
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
                    },
                    |terminal_grid| {
                        let cursor = terminal_grid.grid().cursor();
                        ParsedPane {
                            _pane_id: state.pane_id,
                            pane_index: state.pane_index,
                            focused: state.focused,
                            screen_text: terminal_grid_to_text(
                                terminal_grid.grid(),
                                usize::from(state.rows),
                            ),
                            cursor_row: u16::try_from(cursor.row).unwrap_or(u16::MAX),
                            cursor_col: u16::try_from(cursor.col).unwrap_or(u16::MAX),
                        }
                    },
                )
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

    /// Copy current pane parser state into attach render buffers while preserving renderer caches.
    pub fn sync_attach_render_buffers(
        &self,
        layout: &AttachLayoutState,
        buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    ) {
        buffers.retain(|pane_id, _| self.pane_states.contains_key(pane_id));
        let content_dims = build_pane_content_dimensions_from_scene(&layout.scene);
        for (pane_id, state) in &self.pane_states {
            let (cols, rows) = content_dims
                .get(pane_id)
                .copied()
                .unwrap_or((state.cols, state.rows));
            let buffer = buffers.entry(*pane_id).or_default();
            let screen_text = if let Some(terminal_grid) = state.terminal_grid.as_ref() {
                let snapshot = terminal_grid.snapshot(0, usize::from(rows));
                if let Ok(stream) = bmux_terminal_grid::TerminalGridStream::from_snapshot(
                    &snapshot,
                    bmux_terminal_grid::GridLimits::default(),
                ) {
                    buffer.terminal_grid = stream;
                }
                terminal_grid_to_text(terminal_grid.grid(), usize::from(rows))
            } else {
                screen_to_text(state.parser.screen())
            };
            buffer.parser = parser_from_screen_text(&screen_text, rows, cols);
            buffer.last_alternate_screen = state.parser.screen().alternate_screen();
            buffer.sync_update_in_progress = state.sync_update_active;
            buffer.expected_stream_start = state.expected_stream_start;
        }
    }

    /// Get the visible screen text of a specific pane (by index).
    #[must_use]
    pub fn pane_text(&self, pane_index: u32) -> Option<String> {
        self.panes
            .iter()
            .find(|p| p.pane_index == pane_index)
            .map(|p| p.screen_text.clone())
    }

    /// Get all retained main-screen text for a specific pane (by index).
    #[must_use]
    pub fn pane_scrollback_text(&self, pane_index: u32) -> Option<String> {
        let pane_id = self
            .pane_states
            .values()
            .find(|state| state.pane_index == pane_index)
            .map(|state| state.pane_id)?;
        let grid = self.pane_states.get(&pane_id)?.terminal_grid.as_ref()?;
        Some(terminal_grid_rows_to_text(grid.grid().all_main_rows()))
    }

    /// Get cursor position for a pane (by index). Returns (row, col).
    #[must_use]
    pub fn pane_cursor(&self, pane_index: u32) -> Option<(u16, u16)> {
        self.panes
            .iter()
            .find(|p| p.pane_index == pane_index)
            .map(|p| (p.cursor_row, p.cursor_col))
    }

    /// Capture the state of all panes for a snapshot.
    #[must_use]
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
    #[must_use]
    pub fn capture_all_safe(&self) -> Option<Vec<PaneCapture>> {
        if self.panes.is_empty() {
            None
        } else {
            Some(self.capture_all())
        }
    }

    /// Build cursor and screen deltas against a previous pane cache.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
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
    #[must_use]
    pub fn pane_contains(&self, pane_index: u32, needle: &str) -> bool {
        self.pane_text(pane_index)
            .is_some_and(|text| text.contains(needle))
    }

    /// Check if a pane's screen text matches a regex.
    /// # Errors
    ///
    /// Returns an error if the regex pattern is invalid.
    pub fn pane_matches(&self, pane_index: u32, pattern: &str) -> Result<bool> {
        let re = Regex::new(pattern).with_context(|| format!("invalid regex: {pattern}"))?;
        Ok(self.pane_matches_compiled(pane_index, &re))
    }

    /// Check if a pane's screen text matches a pre-compiled regex.
    /// Use this in hot loops (e.g. `wait-for` polling) to avoid recompiling.
    #[must_use]
    pub fn pane_matches_compiled(&self, pane_index: u32, re: &Regex) -> bool {
        self.pane_text(pane_index)
            .is_some_and(|text| re.is_match(&text))
    }

    /// Resolve which pane index to inspect. If `pane` is `None`, uses the focused pane.
    /// # Errors
    ///
    /// Returns an error if the pane index is out of range.
    #[allow(clippy::unused_self)] // Method on inspector for API consistency; may use self in future
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
        pane_input_modes: Vec::new(),
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

#[allow(clippy::cast_possible_truncation)]
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
        let _ = writeln!(output, "@@ -{},1 +{},1 @@", row + 1, row + 1);
        let _ = writeln!(output, "-{prev}");
        let _ = writeln!(output, "+{curr}");
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
fn build_pane_dimensions_from_scene(
    scene: &bmux_attach_layout_protocol::AttachScene,
) -> BTreeMap<Uuid, (u16, u16)> {
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

/// Extract per-pane content dimensions from scene surface content rects.
fn build_pane_content_dimensions_from_scene(
    scene: &bmux_attach_layout_protocol::AttachScene,
) -> BTreeMap<Uuid, (u16, u16)> {
    scene
        .surfaces
        .iter()
        .filter_map(|surface| {
            let pane_id = surface.pane_id?;
            if !surface.visible {
                return None;
            }
            let cols = surface.content_rect.w.max(1);
            let rows = surface.content_rect.h.max(1);
            Some((pane_id, (cols, rows)))
        })
        .collect()
}

fn terminal_grid_to_text(grid: &bmux_terminal_grid::TerminalGrid, rows: usize) -> String {
    terminal_grid_rows_to_text(grid.display_rows(0, rows))
}

fn terminal_grid_rows_to_text(rows: Vec<bmux_terminal_grid::PhysicalRow>) -> String {
    let mut lines = rows
        .into_iter()
        .map(|row| {
            let mut line = String::new();
            for cell in row.cells() {
                if !cell.is_wide_continuation() {
                    line.push_str(cell.text());
                }
            }
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>();

    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    lines.join("\n")
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

fn parser_from_screen_text(text: &str, rows: u16, cols: u16) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 4_096);
    for (row, line) in text.lines().take(usize::from(rows)).enumerate() {
        let cursor_row = row.saturating_add(1);
        let sequence = format!("\x1b[{cursor_row};1H{line}");
        parser.process(sequence.as_bytes());
    }
    parser
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
    fn parser_from_screen_text_reconstructs_visible_rows_with_requested_size() {
        let parser = parser_from_screen_text("alpha\nbeta", 4, 10);

        assert_eq!(parser.screen().size(), (4, 10));
        assert_eq!(screen_to_text(parser.screen()), "alpha\nbeta");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Inline fixture keeps the render-buffer sync contract visible in one place.
    fn sync_attach_render_buffers_preserves_render_caches_and_uses_scene_dimensions() {
        use bmux_attach_layout_protocol::{
            AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface,
            AttachSurfaceKind, PaneLayoutNode, PaneState, PaneSummary,
        };
        use bmux_attach_pipeline::types::ExtensionRenderCacheEntry;
        use bmux_plugin::{ExtensionRect, RenderDamage};

        let session_id = Uuid::from_u128(91);
        let pane_id = Uuid::from_u128(92);
        let surface_id = Uuid::from_u128(93);
        let mut pane_parser = vt100::Parser::new(5, 12, 4_096);
        pane_parser.process(b"first\r\nsecond");
        let mut inspector = ScreenInspector {
            panes: Vec::new(),
            pane_states: BTreeMap::new(),
            viewport_rows: 8,
            viewport_cols: 20,
            session_id: Some(session_id),
            needs_bootstrap: false,
        };
        inspector.pane_states.insert(
            pane_id,
            PaneStreamState {
                pane_id,
                pane_index: 1,
                focused: true,
                rows: 5,
                cols: 12,
                parser: pane_parser,
                terminal_grid: None,
                expected_stream_start: Some(42),
                sync_update_active: true,
            },
        );

        let mut buffers = BTreeMap::new();
        let mut buffer = PaneRenderBuffer {
            prev_rows: vec!["cached row".to_string()],
            ..PaneRenderBuffer::default()
        };
        buffer.extension_render_cache.insert(
            ("test.extension".to_string(), surface_id),
            ExtensionRenderCacheEntry {
                surface_id,
                surface_rect: ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 6,
                    h: 3,
                },
                damage: RenderDamage::FullSurface,
                revision: 7,
                bytes: b"cached".to_vec(),
            },
        );
        buffers.insert(pane_id, buffer);

        let layout = AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 1,
                name: None,
                focused: true,
                state: PaneState::Running,
                state_reason: None,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: surface_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 8,
                        h: 5,
                    },
                    content_rect: AttachRect {
                        x: 1,
                        y: 1,
                        w: 6,
                        h: 3,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                }],
            },
            zoomed: false,
        };

        inspector.sync_attach_render_buffers(&layout, &mut buffers);

        let synced = buffers.get(&pane_id).expect("buffer should remain");
        assert_eq!(synced.parser.screen().size(), (3, 6));
        assert_eq!(screen_to_text(synced.parser.screen()), "first\nsecond");
        assert_eq!(synced.prev_rows, vec!["cached row".to_string()]);
        assert_eq!(synced.extension_render_cache.len(), 1);
        assert_eq!(synced.expected_stream_start, Some(42));
        assert!(synced.sync_update_in_progress);
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
