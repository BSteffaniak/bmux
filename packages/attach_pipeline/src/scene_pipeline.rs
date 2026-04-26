use crate::cursor::apply_attach_cursor_state;
use crate::reconcile::{
    apply_attach_output_chunk_with, attach_layout_pane_id_set,
    attach_layout_requires_snapshot_hydration, attach_scene_revealed_pane_ids,
    resize_attach_parsers_for_scene_with_size,
};
use crate::render::{append_pane_output, render_attach_scene, visible_scene_pane_ids};
use crate::types::{AttachCursorState, PaneRenderBuffer};
use crate::{mouse_protocol_encoding_to_ipc, mouse_protocol_mode_to_ipc};
use anyhow::Result;
use bmux_attach_pipeline_models::{
    AttachChunkApplyOutcome, AttachOutputChunkMeta, AttachPipelineDiagnosticCode,
    AttachPipelineDiagnosticEvent, AttachViewport,
};
use bmux_client::{AttachLayoutState, AttachPaneSnapshotState, AttachSnapshotState};
use bmux_ipc::{
    AttachInputModeState, AttachMouseProtocolState, AttachPaneChunk, AttachViewComponent,
};
use crossterm::cursor::{Hide, SavePosition};
use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub struct AttachScenePipeline {
    viewport: AttachViewport,
    pub layout_state: Option<AttachLayoutState>,
    pub pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
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
            resize_attach_parsers_for_scene_with_size(
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
            resize_attach_parsers_for_scene_with_size(
                &mut self.pane_buffers,
                &layout_state.scene,
                self.viewport.cols,
                self.viewport.rows,
            );
        }

        for chunk in chunks {
            let pane_id = chunk.pane_id;
            let buffer = self.pane_buffers.entry(pane_id).or_default();
            let _ = append_pane_output(buffer, &chunk.data);
            buffer.sync_update_in_progress = chunk.sync_update_active;
            buffer.expected_stream_start = Some(chunk.stream_end);
            update_parser_mode_hints(
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

    pub fn hydrate_pane_snapshot(&mut self, pane_ids: &[Uuid], snapshot: AttachPaneSnapshotState) {
        let requested = pane_ids.iter().copied().collect::<BTreeSet<_>>();
        for pane_id in pane_ids {
            self.pane_buffers
                .insert(*pane_id, PaneRenderBuffer::default());
        }

        if let Some(layout_state) = self.layout_state.as_ref() {
            resize_attach_parsers_for_scene_with_size(
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
            let _ = append_pane_output(buffer, &chunk.data);
            buffer.sync_update_in_progress = chunk.sync_update_active;
            buffer.expected_stream_start = Some(chunk.stream_end);
            update_parser_mode_hints(
                &mut self.pane_mouse_protocol_hints,
                &mut self.pane_input_mode_hints,
                chunk.pane_id,
                buffer,
            );
            self.dirty_pane_ids.insert(chunk.pane_id);
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
        self.pane_buffers
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        self.pane_mouse_protocol_hints
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        self.pane_input_mode_hints
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        self.layout_state = Some(next_layout);

        if let Some(layout_state) = self.layout_state.as_ref() {
            resize_attach_parsers_for_scene_with_size(
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
                let toggled_alternate = append_pane_output(buffer, bytes);
                update_parser_mode_hints(
                    &mut self.pane_mouse_protocol_hints,
                    &mut self.pane_input_mode_hints,
                    pane_id,
                    buffer,
                );
                if toggled_alternate {
                    self.full_pane_redraw = true;
                }
                !bytes.is_empty()
            },
        );

        match outcome {
            AttachChunkApplyOutcome::Applied { had_data } => {
                if had_data {
                    self.dirty_pane_ids.insert(pane_id);
                }
            }
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

        let mut frame_bytes = Vec::new();
        queue!(frame_bytes, BeginSynchronizedUpdate, SavePosition, Hide)?;
        let cursor_state = render_attach_scene(
            &mut frame_bytes,
            &layout_state.scene,
            &layout_state.panes,
            &mut self.pane_buffers,
            &self.dirty_pane_ids,
            self.full_pane_redraw,
            self.viewport.status_top_inset,
            self.viewport.status_bottom_inset,
            false,
            0,
            None,
            None,
            layout_state.zoomed,
            (self.viewport.cols, self.viewport.rows),
            &bmux_appearance::RuntimeAppearance::default(),
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

fn update_parser_mode_hints(
    pane_mouse_protocol_hints: &mut BTreeMap<Uuid, AttachMouseProtocolState>,
    pane_input_mode_hints: &mut BTreeMap<Uuid, AttachInputModeState>,
    pane_id: Uuid,
    buffer: &PaneRenderBuffer,
) {
    let screen = buffer.parser.screen();
    pane_mouse_protocol_hints.insert(
        pane_id,
        AttachMouseProtocolState {
            mode: mouse_protocol_mode_to_ipc(screen.mouse_protocol_mode()),
            encoding: mouse_protocol_encoding_to_ipc(screen.mouse_protocol_encoding()),
        },
    );
    pane_input_mode_hints.insert(
        pane_id,
        AttachInputModeState {
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
        },
    );
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
