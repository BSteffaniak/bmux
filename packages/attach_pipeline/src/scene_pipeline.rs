use crate::cursor::apply_attach_cursor_state;
use crate::reconcile::{
    apply_attach_output_chunk_with, attach_layout_pane_id_set,
    attach_layout_requires_snapshot_hydration, attach_scene_revealed_pane_ids,
    resize_attach_grids_for_scene_with_size,
};
use crate::render::{
    DamageCoalescingPolicy, FrameDamage, render_attach_scene_with_terminal_graphics_cache,
    visible_scene_pane_ids,
};
use crate::types::{
    AttachCursorState, PaneRenderBuffer, PaneScrollbackViews, TerminalGraphicsCache,
};
use crate::update_protocol_hints_from_state;
use anyhow::{Context, Result};
use bmux_attach_layout_protocol::{
    AttachInputModeState, AttachMouseProtocolState, AttachPaneChunk,
};
use bmux_attach_pipeline_models::{
    AttachChunkApplyOutcome, AttachOutputChunkMeta, AttachPipelineDiagnosticCode,
    AttachPipelineDiagnosticEvent, AttachViewport,
};
use bmux_attach_view_protocol::AttachViewComponent;
use bmux_client::{AttachLayoutState, AttachPaneSnapshotState, AttachSnapshotState};
use bmux_terminal_grid::{GridDeltaBatch, GridLimits, GridMode, GridSnapshot, TerminalGridStream};
use crossterm::cursor::{Hide, SavePosition};
use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachPaneGridSnapshotState {
    pub pane_id: Uuid,
    pub stream_end: u64,
    pub snapshot: GridSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachPaneGridDeltaState {
    pub pane_id: Uuid,
    pub batches: Vec<GridDeltaBatch>,
}

pub struct AttachScenePipeline {
    viewport: AttachViewport,
    pub layout_state: Option<AttachLayoutState>,
    pub pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    hydrated_grid_panes: BTreeSet<Uuid>,
    terminal_graphics_cache: TerminalGraphicsCache,
    pane_mouse_protocol_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pane_input_mode_hints: BTreeMap<Uuid, AttachInputModeState>,
    dirty_pane_ids: BTreeSet<Uuid>,
    full_pane_redraw: bool,
    last_cursor_state: Option<AttachCursorState>,
    diagnostics: VecDeque<AttachPipelineDiagnosticEvent>,
    next_diagnostic_sequence: u64,
    max_diagnostics: usize,
}

impl AttachScenePipeline {
    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn new(viewport: AttachViewport) -> Self {
        Self {
            viewport,
            layout_state: None,
            pane_buffers: BTreeMap::new(),
            hydrated_grid_panes: BTreeSet::new(),
            terminal_graphics_cache: TerminalGraphicsCache::new(),
            pane_mouse_protocol_hints: BTreeMap::new(),
            pane_input_mode_hints: BTreeMap::new(),
            dirty_pane_ids: BTreeSet::new(),
            full_pane_redraw: true,
            last_cursor_state: None,
            diagnostics: VecDeque::new(),
            next_diagnostic_sequence: 1,
            max_diagnostics: 256,
        }
    }

    pub fn set_viewport(&mut self, viewport: AttachViewport) {
        self.viewport = viewport;
        if let Some(layout_state) = self.layout_state.as_ref() {
            resize_attach_grids_for_scene_with_size(
                &mut self.pane_buffers,
                &layout_state.scene,
                viewport.cols,
                viewport.rows,
            );
        }
        self.full_pane_redraw = true;
    }

    pub fn hydrate_snapshot(&mut self, snapshot: AttachSnapshotState) {
        let AttachSnapshotState {
            context_id,
            session_id,
            focused_pane_id,
            panes,
            layout_root,
            scene,
            chunks,
            pane_mouse_protocols: _,
            pane_input_modes: _,
            zoomed,
        } = snapshot;

        self.pane_buffers.clear();
        self.hydrated_grid_panes.clear();
        self.pane_mouse_protocol_hints.clear();
        self.pane_input_mode_hints.clear();
        self.layout_state = Some(AttachLayoutState {
            context_id,
            session_id,
            focused_pane_id,
            panes,
            layout_root,
            scene,
            zoomed,
        });

        if let Some(layout_state) = self.layout_state.as_ref() {
            resize_attach_grids_for_scene_with_size(
                &mut self.pane_buffers,
                &layout_state.scene,
                self.viewport.cols,
                self.viewport.rows,
            );
        }

        for chunk in chunks {
            let pane_id = chunk.pane_id;
            let buffer = self.pane_buffers.entry(pane_id).or_default();
            let _ = buffer.protocol_tracker.process(&chunk.data);
            buffer.sync_update_in_progress = chunk.sync_update_active;
            buffer.expected_stream_start = Some(chunk.stream_end);
            sync_protocol_hints_from_buffer(
                &mut self.pane_mouse_protocol_hints,
                &mut self.pane_input_mode_hints,
                pane_id,
                buffer,
            );
        }

        if let Some(layout_state) = self.layout_state.as_ref() {
            for pane_id in visible_scene_pane_ids(&layout_state.scene) {
                self.dirty_pane_ids.insert(pane_id);
            }
        }
        self.full_pane_redraw = true;
        self.push_diagnostic(
            AttachPipelineDiagnosticCode::SnapshotHydrateFull,
            "hydrated attach scene from full snapshot",
            None,
        );
    }

    /// Hydrate local structured pane grids from server snapshots.
    ///
    /// # Errors
    ///
    /// Returns an error when a snapshot cannot be converted into a valid grid.
    pub fn hydrate_pane_grid_snapshots(
        &mut self,
        snapshots: Vec<AttachPaneGridSnapshotState>,
    ) -> Result<()> {
        for pane_snapshot in snapshots {
            let stream =
                TerminalGridStream::from_snapshot(&pane_snapshot.snapshot, GridLimits::default())
                    .with_context(|| {
                    format!(
                        "hydrating grid snapshot for pane {} (revision {}, stream end {})",
                        pane_snapshot.pane_id,
                        pane_snapshot.snapshot.revision,
                        pane_snapshot.stream_end
                    )
                })?;
            let protocol = stream.grid().protocol_state();
            let alternate_screen = stream.grid().mode() == GridMode::Alternate;
            let buffer = self.pane_buffers.entry(pane_snapshot.pane_id).or_default();
            buffer.terminal_grid = stream;
            buffer.visual_row_fingerprints.clear();
            buffer.protocol_tracker.restore(
                protocol,
                alternate_screen,
                &pane_snapshot.snapshot.pending_bytes,
            );
            buffer.expected_stream_start = Some(pane_snapshot.stream_end);
            buffer.prev_rows.clear();
            sync_protocol_hints_from_buffer(
                &mut self.pane_mouse_protocol_hints,
                &mut self.pane_input_mode_hints,
                pane_snapshot.pane_id,
                buffer,
            );
            self.hydrated_grid_panes.insert(pane_snapshot.pane_id);
            self.dirty_pane_ids.insert(pane_snapshot.pane_id);
            self.full_pane_redraw = true;
        }
        Ok(())
    }

    pub fn hydrate_pane_snapshot(&mut self, pane_ids: &[Uuid], snapshot: AttachPaneSnapshotState) {
        let requested = pane_ids.iter().copied().collect::<BTreeSet<_>>();
        for pane_id in pane_ids {
            self.hydrated_grid_panes.remove(pane_id);
            self.pane_mouse_protocol_hints.remove(pane_id);
            self.pane_input_mode_hints.remove(pane_id);
            self.pane_buffers
                .insert(*pane_id, PaneRenderBuffer::default());
            self.dirty_pane_ids.insert(*pane_id);
            self.full_pane_redraw = true;
        }

        if let Some(layout_state) = self.layout_state.as_ref() {
            resize_attach_grids_for_scene_with_size(
                &mut self.pane_buffers,
                &layout_state.scene,
                self.viewport.cols,
                self.viewport.rows,
            );
        }

        for chunk in snapshot.chunks {
            if !requested.contains(&chunk.pane_id) {
                continue;
            }
            let buffer = self.pane_buffers.entry(chunk.pane_id).or_default();
            let _ = buffer.protocol_tracker.process(&chunk.data);
            buffer.sync_update_in_progress = chunk.sync_update_active;
            buffer.expected_stream_start = Some(chunk.stream_end);
            sync_protocol_hints_from_buffer(
                &mut self.pane_mouse_protocol_hints,
                &mut self.pane_input_mode_hints,
                chunk.pane_id,
                buffer,
            );
        }

        self.push_diagnostic(
            AttachPipelineDiagnosticCode::SnapshotHydratePane,
            "hydrated pane snapshot after desync",
            None,
        );
    }

    pub fn apply_layout_state(&mut self, next_layout: AttachLayoutState) -> bool {
        let mut requires_snapshot_hydration = false;

        if let Some(previous_layout) = self.layout_state.as_ref() {
            requires_snapshot_hydration =
                attach_layout_requires_snapshot_hydration(previous_layout, &next_layout);
            if previous_layout.scene != next_layout.scene {
                let revealed =
                    attach_scene_revealed_pane_ids(&previous_layout.scene, &next_layout.scene);
                for pane_id in revealed {
                    self.dirty_pane_ids.insert(pane_id);
                }
                self.full_pane_redraw = true;
            } else if previous_layout.focused_pane_id != next_layout.focused_pane_id {
                self.dirty_pane_ids.insert(previous_layout.focused_pane_id);
                self.dirty_pane_ids.insert(next_layout.focused_pane_id);
            }
        } else {
            self.full_pane_redraw = true;
        }

        let active_pane_ids = attach_layout_pane_id_set(&next_layout);
        self.hydrated_grid_panes
            .retain(|pane_id| active_pane_ids.contains(pane_id));
        self.pane_buffers
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        self.pane_mouse_protocol_hints
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        self.pane_input_mode_hints
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        self.layout_state = Some(next_layout);

        if let Some(layout_state) = self.layout_state.as_ref() {
            resize_attach_grids_for_scene_with_size(
                &mut self.pane_buffers,
                &layout_state.scene,
                self.viewport.cols,
                self.viewport.rows,
            );
            for pane_id in visible_scene_pane_ids(&layout_state.scene) {
                self.dirty_pane_ids.insert(pane_id);
            }
        }

        requires_snapshot_hydration
    }

    pub fn apply_view_change_components(&mut self, components: &[AttachViewComponent]) -> bool {
        let mut needs_hydration = false;
        for component in components {
            if matches!(
                component,
                AttachViewComponent::Scene
                    | AttachViewComponent::Layout
                    | AttachViewComponent::SurfaceContent
            ) {
                self.full_pane_redraw = true;
                needs_hydration = true;
            }
        }
        if needs_hydration {
            self.push_diagnostic(
                AttachPipelineDiagnosticCode::ViewChangedHydrate,
                "attach view changed; snapshot hydration requested",
                None,
            );
        }
        needs_hydration
    }

    pub fn apply_chunk(&mut self, chunk: &AttachPaneChunk) -> AttachChunkApplyOutcome {
        let pane_id = chunk.pane_id;
        let outcome = apply_attach_output_chunk_with(
            &mut self.pane_buffers,
            pane_id,
            &chunk.data,
            AttachOutputChunkMeta {
                stream_start: chunk.stream_start,
                stream_end: chunk.stream_end,
                stream_gap: chunk.stream_gap,
                sync_update_active: chunk.sync_update_active,
            },
            |buffer, bytes| {
                let protocol_outcome = buffer.protocol_tracker.process(bytes);
                sync_protocol_hints_from_buffer(
                    &mut self.pane_mouse_protocol_hints,
                    &mut self.pane_input_mode_hints,
                    pane_id,
                    buffer,
                );
                if protocol_outcome.toggled_alternate {
                    self.full_pane_redraw = true;
                }
                !bytes.is_empty()
            },
        );

        match outcome {
            AttachChunkApplyOutcome::Applied { .. } => {}
            AttachChunkApplyOutcome::Stale => {
                self.push_diagnostic(
                    AttachPipelineDiagnosticCode::ChunkStale,
                    format!("ignored stale chunk for pane {pane_id}"),
                    Some(pane_id),
                );
            }
            AttachChunkApplyOutcome::Desync => {
                self.push_diagnostic(
                    AttachPipelineDiagnosticCode::ChunkDesync,
                    format!("detected stream desync for pane {pane_id}"),
                    Some(pane_id),
                );
            }
        }

        outcome
    }

    /// Apply authoritative structured terminal-grid deltas.
    ///
    /// # Errors
    ///
    /// Returns an error when any delta cannot be applied to the local grid.
    pub fn apply_pane_grid_deltas(&mut self, deltas: Vec<AttachPaneGridDeltaState>) -> Result<()> {
        for pane_delta in deltas {
            if pane_delta.batches.is_empty() {
                continue;
            }
            anyhow::ensure!(
                self.hydrated_grid_panes.contains(&pane_delta.pane_id),
                "grid delta for unhydrated pane {}; snapshot required",
                pane_delta.pane_id,
            );
            let buffer = self
                .pane_buffers
                .get_mut(&pane_delta.pane_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "grid delta for unknown pane {}; snapshot required",
                        pane_delta.pane_id
                    )
                })?;
            for batch in &pane_delta.batches {
                let previous_mode = buffer.terminal_grid.grid().mode();
                buffer
                    .terminal_grid
                    .apply_delta(batch, GridLimits::default())
                    .with_context(|| {
                        format!(
                            "applying grid delta for pane {} (revision {} -> {})",
                            pane_delta.pane_id, batch.base_revision, batch.revision
                        )
                    })?;
                if buffer.terminal_grid.grid().mode() != previous_mode {
                    self.full_pane_redraw = true;
                }
                let updated_rows = if batch.reset_rows {
                    Vec::new()
                } else {
                    batch
                        .row_updates
                        .iter()
                        .map(|update| update.row_index)
                        .collect::<Vec<_>>()
                };
                buffer.visual_row_fingerprints.invalidate_rows(
                    batch.reset_rows,
                    batch.content_revision,
                    &updated_rows,
                );
                // Commit metadata with each accepted batch: a later malformed
                // batch must not leave this revision invisible to the renderer.
                let protocol = buffer.terminal_grid.grid().protocol_state();
                let alternate_screen = buffer.terminal_grid.grid().mode() == GridMode::Alternate;
                buffer
                    .protocol_tracker
                    .restore(protocol, alternate_screen, &batch.pending_bytes);
                buffer.prev_rows.clear();
                sync_protocol_hints_from_buffer(
                    &mut self.pane_mouse_protocol_hints,
                    &mut self.pane_input_mode_hints,
                    pane_delta.pane_id,
                    buffer,
                );
                self.dirty_pane_ids.insert(pane_delta.pane_id);
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn pane_grid_revisions(&self, pane_ids: &[Uuid]) -> Vec<u64> {
        pane_ids
            .iter()
            .map(|pane_id| {
                if !self.hydrated_grid_panes.contains(pane_id) {
                    return 0;
                }
                self.pane_buffers
                    .get(pane_id)
                    .map_or(0, |buffer| buffer.terminal_grid.grid().revision())
            })
            .collect()
    }

    /// Render a composed frame when any pane/layout state is dirty.
    ///
    /// # Errors
    ///
    /// Returns an error when frame composition or ANSI queueing fails.
    pub fn render_frame(&mut self) -> Result<Option<Vec<u8>>> {
        if self.layout_state.is_none() {
            return Ok(None);
        }

        let should_render = self.full_pane_redraw || !self.dirty_pane_ids.is_empty();
        if !should_render {
            return Ok(None);
        }

        let Some(layout_state) = self.layout_state.as_ref() else {
            return Ok(None);
        };

        let frame_damage = if self.full_pane_redraw {
            FrameDamage::full_frame()
        } else {
            let mut damage = FrameDamage::default();
            for pane_id in &self.dirty_pane_ids {
                damage.mark_content_surface(*pane_id);
            }
            damage
        };

        let mut frame_bytes = Vec::new();
        queue!(frame_bytes, BeginSynchronizedUpdate, SavePosition, Hide)?;
        let cursor_state = render_attach_scene_with_terminal_graphics_cache(
            &mut frame_bytes,
            &layout_state.scene,
            &layout_state.panes,
            &mut self.pane_buffers,
            &mut self.terminal_graphics_cache,
            &frame_damage,
            self.viewport.top_inset,
            self.viewport.bottom_inset,
            // Snapshot-mode scene pipeline renders live pane output only; it has
            // no per-pane scrollback views.
            &PaneScrollbackViews::new(),
            layout_state.zoomed,
            (self.viewport.cols, self.viewport.rows),
            &bmux_appearance::RuntimeAppearance::default(),
            DamageCoalescingPolicy::default(),
            &[],
        )?;
        apply_attach_cursor_state(
            &mut frame_bytes,
            cursor_state,
            &mut self.last_cursor_state,
            false,
        )?;
        queue!(frame_bytes, EndSynchronizedUpdate)?;

        self.full_pane_redraw = false;
        self.dirty_pane_ids.clear();
        Ok(Some(frame_bytes))
    }

    #[must_use]
    pub const fn pane_mouse_protocol_hints(&self) -> &BTreeMap<Uuid, AttachMouseProtocolState> {
        &self.pane_mouse_protocol_hints
    }

    #[must_use]
    pub const fn pane_input_mode_hints(&self) -> &BTreeMap<Uuid, AttachInputModeState> {
        &self.pane_input_mode_hints
    }

    #[must_use]
    pub fn drain_diagnostics(
        &mut self,
        since_sequence: Option<u64>,
        limit: usize,
    ) -> Vec<AttachPipelineDiagnosticEvent> {
        self.diagnostics
            .iter()
            .filter(|event| since_sequence.is_none_or(|since| event.sequence > since))
            .take(limit)
            .cloned()
            .collect()
    }

    fn push_diagnostic(
        &mut self,
        code: AttachPipelineDiagnosticCode,
        message: impl Into<String>,
        pane_id: Option<Uuid>,
    ) {
        let event = AttachPipelineDiagnosticEvent {
            sequence: self.next_diagnostic_sequence,
            timestamp_ms: now_epoch_ms(),
            code,
            message: message.into(),
            pane_id,
        };
        self.next_diagnostic_sequence = self.next_diagnostic_sequence.saturating_add(1);
        self.diagnostics.push_back(event);
        while self.diagnostics.len() > self.max_diagnostics {
            let _ = self.diagnostics.pop_front();
        }
    }
}

fn sync_protocol_hints_from_buffer(
    pane_mouse_protocol_hints: &mut BTreeMap<Uuid, AttachMouseProtocolState>,
    pane_input_mode_hints: &mut BTreeMap<Uuid, AttachInputModeState>,
    pane_id: Uuid,
    buffer: &PaneRenderBuffer,
) {
    update_protocol_hints_from_state(
        pane_mouse_protocol_hints,
        pane_input_mode_hints,
        pane_id,
        buffer.protocol_tracker.protocol_state(),
    );
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_history_survives_updates_and_alternate_exit() {
        let pane_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 20,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let mut producer = TerminalGridStream::new(20, 2, GridLimits::default()).unwrap();
        producer.process(b"history\r\nsecond\r\nthird\x1b[?1049halt");
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 100,
                snapshot: producer.snapshot(0, 3),
            }])
            .unwrap();
        let sparse = producer.process_delta(b" update").unwrap();
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![sparse],
            }])
            .unwrap();
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 3),
            producer.snapshot(0, 3)
        );
        let before = producer.snapshot(0, 3);
        let incomplete = producer.process_delta(b"\x1b[?1049l").unwrap();
        let revision = pipeline.pane_grid_revisions(&[pane_id]);
        assert!(
            pipeline
                .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                    pane_id,
                    batches: vec![incomplete],
                }])
                .is_err()
        );
        assert_eq!(pipeline.pane_grid_revisions(&[pane_id]), revision);
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 3),
            before
        );
        let replacement = GridDeltaBatch::between(&before, &producer.snapshot(0, 3)).unwrap();
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![replacement],
            }])
            .unwrap();
        let sparse = producer.process_delta(b"!").unwrap();
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![sparse],
            }])
            .unwrap();
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 3),
            producer.snapshot(0, 3)
        );
        assert_eq!(
            pipeline.pane_grid_revisions(&[pane_id]),
            vec![producer.grid().revision()]
        );
    }

    #[test]
    fn hydrate_pane_grid_snapshot_preserves_pending_escape_bytes() {
        let pane_id = Uuid::new_v4();
        let mut source = TerminalGridStream::new(10, 2, GridLimits::default())
            .expect("source grid dimensions are valid");
        source.process(b"A\x1b[");
        let snapshot = source.snapshot(0, source.grid().height());
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });

        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 2,
                snapshot,
            }])
            .expect("snapshot should hydrate");
        pipeline
            .pane_buffers
            .get_mut(&pane_id)
            .expect("pane buffer should exist")
            .terminal_grid
            .process(b"31mR");

        let grid = pipeline
            .pane_buffers
            .get(&pane_id)
            .expect("pane buffer should exist")
            .terminal_grid
            .grid();
        let rows = grid.viewport_rows();
        let text = rows[0]
            .cells()
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .map(bmux_terminal_grid::Cell::text)
            .collect::<String>()
            .trim_end()
            .to_string();
        let red = rows[0].cells()[1].style();

        assert_eq!(text, "AR");
        assert_eq!(
            grid.palette().get(red).fg,
            Some(bmux_terminal_grid::Color::Indexed(1))
        );
    }

    #[test]
    fn empty_raw_resync_clears_only_requested_protocol_hints() {
        let pane_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        producer.process(b"\x1b[?1000h\x1b[?2004h\x1b[?1h");
        let snapshot = producer.snapshot(0, 2);
        for id in [pane_id, other_id] {
            pipeline
                .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                    pane_id: id,
                    stream_end: 24,
                    snapshot: snapshot.clone(),
                }])
                .unwrap();
        }
        let mouse = pipeline.pane_mouse_protocol_hints[&other_id];
        let input = pipeline.pane_input_mode_hints[&other_id];
        assert!(pipeline.pane_mouse_protocol_hints.contains_key(&pane_id));
        assert!(pipeline.pane_input_mode_hints.contains_key(&pane_id));
        pipeline.dirty_pane_ids.clear();
        pipeline.full_pane_redraw = false;
        pipeline.hydrate_pane_snapshot(
            &[pane_id],
            AttachPaneSnapshotState {
                chunks: vec![],
                pane_mouse_protocols: vec![],
                pane_input_modes: vec![],
            },
        );
        assert_eq!(pipeline.dirty_pane_ids, BTreeSet::from([pane_id]));
        assert!(pipeline.full_pane_redraw);
        assert!(!pipeline.pane_mouse_protocol_hints.contains_key(&pane_id));
        assert!(!pipeline.pane_input_mode_hints.contains_key(&pane_id));
        assert_eq!(pipeline.pane_mouse_protocol_hints[&other_id], mouse);
        assert_eq!(pipeline.pane_input_mode_hints[&other_id], input);
        assert_eq!(
            pipeline.pane_grid_revisions(&[pane_id, other_id]),
            vec![0, snapshot.revision]
        );
    }

    #[test]
    fn partial_snapshot_hydration_marks_accepted_pane_for_full_redraw() {
        let pane_id = Uuid::new_v4();
        let rejected_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        pipeline.full_pane_redraw = false;
        pipeline.hydrate_pane_grid_snapshots(vec![]).unwrap();
        assert!(!pipeline.full_pane_redraw);
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        producer.process(b"main\x1b[?1049halt");
        let snapshot = producer.snapshot(0, 2);
        let mut invalid = snapshot.clone();
        invalid.height = 0;
        let error = pipeline
            .hydrate_pane_grid_snapshots(vec![
                AttachPaneGridSnapshotState {
                    pane_id,
                    stream_end: 20,
                    snapshot: snapshot.clone(),
                },
                AttachPaneGridSnapshotState {
                    pane_id: rejected_id,
                    stream_end: 20,
                    snapshot: invalid,
                },
            ])
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<bmux_terminal_grid::TerminalGridError>(),
            Some(bmux_terminal_grid::TerminalGridError::ZeroDimensions)
        ));
        assert!(pipeline.full_pane_redraw);
        assert!(pipeline.dirty_pane_ids.contains(&pane_id));
        assert!(!pipeline.pane_buffers.contains_key(&rejected_id));
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 2),
            snapshot
        );
        assert_eq!(
            pipeline.pane_grid_revisions(&[pane_id, rejected_id]),
            vec![snapshot.revision, 0]
        );
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id: rejected_id,
                stream_end: 20,
                snapshot: snapshot.clone(),
            }])
            .unwrap();
        assert_eq!(
            pipeline.pane_grid_revisions(&[pane_id, rejected_id]),
            vec![snapshot.revision, snapshot.revision]
        );
        for id in [pane_id, rejected_id] {
            assert_eq!(
                pipeline.pane_buffers[&id].terminal_grid.snapshot(0, 2),
                snapshot
            );
            assert_eq!(pipeline.pane_buffers[&id].expected_stream_start, Some(20));
        }
        let delta = producer.process_delta(b"\x1b[?1049l restored").unwrap();
        pipeline
            .apply_pane_grid_deltas(
                [pane_id, rejected_id]
                    .into_iter()
                    .map(|pane_id| AttachPaneGridDeltaState {
                        pane_id,
                        batches: vec![delta.clone()],
                    })
                    .collect(),
            )
            .unwrap();
        for id in [pane_id, rejected_id] {
            assert_eq!(
                pipeline.pane_buffers[&id].terminal_grid.snapshot(0, 2),
                producer.snapshot(0, 2)
            );
            assert_eq!(pipeline.pane_buffers[&id].expected_stream_start, Some(20));
        }
    }

    #[test]
    fn invalid_replacement_preserves_hydrated_replica_and_continuity() {
        let pane_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        producer.process(b"main\x1b[?1049halt\x1b[?1000");
        let snapshot = producer.snapshot(0, 2);
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 24,
                snapshot: snapshot.clone(),
            }])
            .unwrap();
        let mut invalid = snapshot.clone();
        invalid.mode = "invalid".to_owned();
        invalid.pending_bytes.clear();
        assert!(
            pipeline
                .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                    pane_id,
                    stream_end: 100,
                    snapshot: invalid,
                }])
                .is_err()
        );
        let buffer = &pipeline.pane_buffers[&pane_id];
        assert_eq!(buffer.terminal_grid.snapshot(0, 2), snapshot);
        assert_eq!(buffer.expected_stream_start, Some(24));
        assert_eq!(
            pipeline.pane_grid_revisions(&[pane_id]),
            vec![snapshot.revision]
        );
        let batch = producer.process_delta(b"h").unwrap();
        let tracker = &mut pipeline
            .pane_buffers
            .get_mut(&pane_id)
            .unwrap()
            .protocol_tracker;
        let _ = tracker.process(b"h");
        assert_eq!(tracker.protocol_state(), producer.grid().protocol_state());
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![batch],
            }])
            .unwrap();
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 2),
            producer.snapshot(0, 2)
        );
    }

    #[test]
    fn raw_resync_invalidates_structured_hydration_until_replaced() {
        let pane_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        producer.process(b"before");
        let snapshot = producer.snapshot(0, 2);
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 6,
                snapshot: snapshot.clone(),
            }])
            .unwrap();
        let batch = producer.process_delta(b" after").unwrap();
        pipeline.hydrate_pane_snapshot(
            &[pane_id],
            AttachPaneSnapshotState {
                chunks: vec![],
                pane_mouse_protocols: vec![],
                pane_input_modes: vec![],
            },
        );
        assert_eq!(pipeline.pane_grid_revisions(&[pane_id]), vec![0]);
        assert!(
            pipeline
                .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                    pane_id,
                    batches: vec![batch.clone()],
                }])
                .is_err()
        );
        // Failed replacement must not promote the new default buffer to hydrated.
        let mut invalid = snapshot.clone();
        invalid.width = 0;
        assert!(
            pipeline
                .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                    pane_id,
                    stream_end: 6,
                    snapshot: invalid,
                }])
                .is_err()
        );
        assert_eq!(pipeline.pane_grid_revisions(&[pane_id]), vec![0]);
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 6,
                snapshot,
            }])
            .unwrap();
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![batch],
            }])
            .unwrap();
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 2),
            producer.snapshot(0, 2)
        );
    }

    #[test]
    fn unhydrated_local_grid_does_not_advertise_replication_revision() {
        let pane_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let buffer = pipeline.pane_buffers.entry(pane_id).or_default();
        buffer.terminal_grid.resize(10, 2).unwrap();
        buffer.terminal_grid.process(b"local");
        assert!(buffer.terminal_grid.grid().revision() > 0);
        assert_eq!(pipeline.pane_grid_revisions(&[pane_id]), vec![0]);
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        producer.process(b"authoritative");
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 13,
                snapshot: producer.snapshot(0, 2),
            }])
            .unwrap();
        assert_eq!(
            pipeline.pane_grid_revisions(&[pane_id, Uuid::new_v4()]),
            vec![producer.grid().revision(), 0]
        );
    }

    #[test]
    fn delta_for_unknown_pane_does_not_create_buffer() {
        let pane_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![],
            }])
            .unwrap();
        assert!(!pipeline.pane_buffers.contains_key(&pane_id));
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let snapshot = producer.snapshot(0, 2);
        let batch = producer.process_delta(b"hello").unwrap();
        assert!(
            pipeline
                .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                    pane_id,
                    batches: vec![batch.clone()],
                }])
                .is_err()
        );
        assert!(!pipeline.pane_buffers.contains_key(&pane_id));
        pipeline
            .pane_buffers
            .insert(pane_id, PaneRenderBuffer::default());
        assert!(
            pipeline
                .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                    pane_id,
                    batches: vec![batch.clone()],
                }])
                .is_err()
        );
        assert!(!pipeline.hydrated_grid_panes.contains(&pane_id));
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 0,
                snapshot,
            }])
            .unwrap();
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![batch],
            }])
            .unwrap();
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 2),
            producer.snapshot(0, 2)
        );
    }

    #[test]
    fn snapshot_restores_protocol_parser_prefix_and_discards_old_prefix() {
        let pane_id = Uuid::new_v4();
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let _ = pipeline
            .pane_buffers
            .entry(pane_id)
            .or_default()
            .protocol_tracker
            .process(b"\x1b]old title");
        producer.process(b"\x1b[?1000");
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 7,
                snapshot: producer.snapshot(0, 2),
            }])
            .unwrap();
        producer.process(b"h");
        let tracker = &mut pipeline
            .pane_buffers
            .get_mut(&pane_id)
            .unwrap()
            .protocol_tracker;
        let _ = tracker.process(b"h");
        assert_eq!(tracker.protocol_state(), producer.grid().protocol_state());
        assert_ne!(
            tracker.protocol_state(),
            bmux_terminal_grid::ProtocolState::default()
        );
    }

    #[test]
    fn grid_delta_restores_protocol_parser_prefix() {
        let pane_id = Uuid::new_v4();
        let mut producer =
            bmux_terminal_grid::TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 0,
                snapshot: producer.snapshot(0, 2),
            }])
            .unwrap();
        pipeline
            .pane_buffers
            .get_mut(&pane_id)
            .unwrap()
            .protocol_tracker
            .process(b"\x1b]stale title");
        let delta = producer.process_delta(b"\x1b[?1000").unwrap();
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![delta.clone()],
            }])
            .unwrap();
        assert!(
            pipeline
                .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                    pane_id,
                    batches: vec![delta],
                }])
                .is_err()
        );
        assert_eq!(
            pipeline.pane_grid_revisions(&[pane_id]),
            vec![producer.grid().revision()]
        );
        producer.process(b"h");
        let tracker = &mut pipeline
            .pane_buffers
            .get_mut(&pane_id)
            .unwrap()
            .protocol_tracker;
        let _ = tracker.process(b"h");
        assert_eq!(tracker.protocol_state(), producer.grid().protocol_state());
        assert_ne!(
            tracker.protocol_state(),
            bmux_terminal_grid::ProtocolState::default()
        );
    }

    #[test]
    fn malformed_delta_suffix_preserves_accepted_batch_render_state() {
        let pane_id = Uuid::new_v4();
        let mut producer =
            bmux_terminal_grid::TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        pipeline
            .hydrate_pane_grid_snapshots(vec![AttachPaneGridSnapshotState {
                pane_id,
                stream_end: 0,
                snapshot: producer.snapshot(0, 2),
            }])
            .unwrap();
        pipeline.dirty_pane_ids.clear();
        pipeline.full_pane_redraw = false;
        let first = producer
            .process_delta(b"\x1b[?1049h\x1b[?1000halt")
            .unwrap();
        let second = producer.process_delta(b" next").unwrap();
        let mut invalid = second.clone();
        invalid.base_revision = 0;
        let error = pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![first.clone(), invalid],
            }])
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<bmux_terminal_grid::TerminalGridStreamDeltaError>(),
            Some(bmux_terminal_grid::TerminalGridStreamDeltaError::Delta(
                bmux_terminal_grid::GridDeltaApplyError::RevisionMismatch {
                    expected: 0,
                    actual,
                }
            )) if *actual == first.revision
        ));
        let buffer = &pipeline.pane_buffers[&pane_id];
        assert_eq!(buffer.terminal_grid.grid().revision(), first.revision);
        assert_eq!(buffer.protocol_tracker.protocol_state(), first.protocol);
        assert!(buffer.protocol_tracker.alternate_screen());
        assert!(pipeline.full_pane_redraw);
        assert!(pipeline.dirty_pane_ids.contains(&pane_id));
        pipeline.full_pane_redraw = false;
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![second],
            }])
            .unwrap();
        assert!(!pipeline.full_pane_redraw);
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 2),
            producer.snapshot(0, 2)
        );
        let exit = producer.process_delta(b"\x1b[?1049lmain").unwrap();
        pipeline
            .apply_pane_grid_deltas(vec![AttachPaneGridDeltaState {
                pane_id,
                batches: vec![exit],
            }])
            .unwrap();
        assert!(pipeline.full_pane_redraw);
        assert!(
            !pipeline.pane_buffers[&pane_id]
                .protocol_tracker
                .alternate_screen()
        );
        assert_eq!(
            pipeline.pane_buffers[&pane_id].terminal_grid.snapshot(0, 2),
            producer.snapshot(0, 2)
        );
    }

    #[test]
    fn raw_chunk_updates_continuity_and_protocol_without_mutating_grid() {
        let pane_id = Uuid::new_v4();
        let mut pipeline = AttachScenePipeline::new(AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let initial_revision = pipeline
            .pane_buffers
            .entry(pane_id)
            .or_default()
            .terminal_grid
            .grid()
            .revision();

        let data = b"render text must not enter the grid\x1b[?1000h".to_vec();
        let stream_end = u64::try_from(data.len()).expect("test data length fits u64");
        let outcome = pipeline.apply_chunk(&AttachPaneChunk {
            pane_id,
            data,
            stream_start: 0,
            stream_end,
            stream_gap: false,
            sync_update_active: false,
        });

        assert_eq!(outcome, AttachChunkApplyOutcome::Applied { had_data: true });
        let buffer = pipeline
            .pane_buffers
            .get(&pane_id)
            .expect("pane buffer exists");
        assert_eq!(buffer.expected_stream_start, Some(stream_end));
        assert_eq!(buffer.terminal_grid.grid().revision(), initial_revision);
        assert_eq!(buffer.terminal_grid.grid().viewport_rows()[0].cells(), &[]);
        assert_eq!(
            pipeline
                .pane_mouse_protocol_hints()
                .get(&pane_id)
                .map(|hint| hint.mode),
            Some(bmux_attach_layout_protocol::AttachMouseProtocolMode::PressRelease)
        );
    }
}
