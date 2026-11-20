//! Provider-neutral parser state shared by terminal and native frontends.
//!
//! Stream frames keep each pane's replica grid level with its source; this module neither
//! executes PTYs nor constructs an alternate layout, image registry, or hit map.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use bmux_attach_layout_protocol::AttachScene;
use bmux_attach_pipeline::PaneRenderBuffer;
use bmux_attach_pipeline::render::apply_pane_frame;
use bmux_client::{
    AttachContinuityValidator, AttachProviderChange, AttachProviderDelta, AttachProviderSnapshot,
    AttachResumeState, AttachStreamCursor,
};
use uuid::Uuid;

/// Validated execution view. Presentation/interaction authority is separate.
pub struct AttachState {
    continuity: AttachContinuityValidator,
    scene: AttachScene,
    buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    poisoned: bool,
}

impl AttachState {
    /// Validate a complete repair snapshot before making it observable.
    pub fn from_snapshot(snapshot: &AttachProviderSnapshot) -> Result<Self> {
        let mut continuity = AttachContinuityValidator::default();
        continuity.apply_snapshot(snapshot)?;
        let buffers = pane_buffers_from_snapshot(snapshot)?;
        Ok(Self {
            continuity,
            scene: snapshot.scene.clone(),
            buffers,
            poisoned: false,
        })
    }

    /// Apply an ordered delta. A parser failure fences the state until repair.
    /// Continuity failures are rejected before any parser bytes are applied.
    pub fn apply_delta(&mut self, delta: &AttachProviderDelta) -> Result<()> {
        anyhow::ensure!(
            !self.poisoned,
            "attach parser state requires a repair snapshot"
        );
        self.continuity.apply_delta(delta)?;
        let scene = delta
            .changes
            .iter()
            .rev()
            .find_map(|change| match change {
                AttachProviderChange::Scene(scene) => Some(scene.clone()),
                _ => None,
            })
            .unwrap_or_else(|| self.scene.clone());
        if let Err(error) = apply_delta_to_buffers(&scene, &mut self.buffers, delta) {
            self.poisoned = true;
            return Err(error);
        }
        self.scene = scene;
        Ok(())
    }

    #[must_use]
    pub fn scene(&self) -> &AttachScene {
        &self.scene
    }

    /// Existing retained compositor input, not an independently composed view.
    pub fn buffers_mut(&mut self) -> &mut BTreeMap<Uuid, PaneRenderBuffer> {
        &mut self.buffers
    }

    #[must_use]
    pub fn resume_state(&self) -> AttachResumeState {
        self.continuity.resume_state()
    }

    #[must_use]
    pub fn focused_stream(&self) -> Option<AttachStreamCursor> {
        let surface_id = match self.scene.focus {
            bmux_attach_layout_protocol::AttachFocusTarget::Pane { pane_id } => pane_id,
            bmux_attach_layout_protocol::AttachFocusTarget::Surface { surface_id } => surface_id,
            bmux_attach_layout_protocol::AttachFocusTarget::None => return None,
        };
        self.resume_state()
            .streams
            .into_iter()
            .find(|stream| stream.surface_id == surface_id)
    }

    pub(crate) fn continuity(&self) -> &AttachContinuityValidator {
        &self.continuity
    }
}

/// Build replica grids from a snapshot's reset frames. Shared with the CLI adapter.
pub fn pane_buffers_from_snapshot(
    snapshot: &AttachProviderSnapshot,
) -> Result<BTreeMap<Uuid, PaneRenderBuffer>> {
    let mut buffers = BTreeMap::new();
    for stream in &snapshot.streams {
        let mut buffer = render_buffer(&snapshot.scene, stream.cursor.surface_id)?;
        apply_stream_frame(&mut buffer, &stream.frame, stream.sync_update_active)?;
        buffers.insert(stream.cursor.surface_id, buffer);
    }
    Ok(buffers)
}

/// Apply one stream frame to its replica.
fn apply_stream_frame(
    buffer: &mut PaneRenderBuffer,
    frame: &[u8],
    sync_update_active: bool,
) -> Result<()> {
    apply_pane_frame(buffer, frame)
        .map_err(|error| anyhow::anyhow!("attach provider frame does not apply: {error}"))?;
    buffer.set_sync_update(sync_update_active, std::time::Instant::now());
    Ok(())
}

/// Allocate a parser using execution-cell geometry, never native pixel geometry.
pub fn render_buffer(scene: &AttachScene, surface_id: Uuid) -> Result<PaneRenderBuffer> {
    let surface = scene
        .surfaces
        .iter()
        .find(|surface| surface.id == surface_id)
        .with_context(|| format!("attach provider stream surface {surface_id} is missing"))?;
    let mut buffer = PaneRenderBuffer::default();
    buffer
        .terminal_grid
        .resize(surface.content_rect.w.max(1), surface.content_rect.h.max(1))
        .map_err(|error| anyhow::anyhow!("attach provider surface grid is invalid: {error}"))?;
    Ok(buffer)
}

/// Apply already validated provider changes to the shared parser buffers.
pub fn apply_delta_to_buffers(
    scene: &AttachScene,
    buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    delta: &AttachProviderDelta,
) -> Result<()> {
    for change in &delta.changes {
        match change {
            AttachProviderChange::StreamFrame(frame) => {
                let surface_id = frame.cursor.surface_id;
                if frame.reset {
                    // Whatever the replica held, it starts over.
                    buffers.insert(surface_id, render_buffer(scene, surface_id)?);
                }
                let buffer = buffers.get_mut(&surface_id).with_context(|| {
                    format!("attach provider frame for unknown surface {surface_id}")
                })?;
                apply_stream_frame(buffer, &frame.frame, frame.sync_update_active)?;
            }
            // Replicas take their size from their frames, not from the scene.
            AttachProviderChange::Scene(_) => {}
            AttachProviderChange::StreamRemoved { stream_id } => {
                if let Some(cursor) = delta
                    .resume
                    .streams
                    .iter()
                    .find(|cursor| &cursor.stream_id == stream_id)
                {
                    buffers.remove(&cursor.surface_id);
                }
            }
            AttachProviderChange::Status { .. } => {}
        }
    }
    let visible = scene
        .surfaces
        .iter()
        .map(|surface| surface.id)
        .collect::<BTreeSet<_>>();
    let current_stream_surfaces = delta
        .resume
        .streams
        .iter()
        .map(|cursor| cursor.surface_id)
        .collect::<BTreeSet<_>>();
    buffers.retain(|id, _| visible.contains(id) && current_stream_surfaces.contains(id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_client::{AttachDeltaSequence, AttachViewRevision};

    fn snapshot() -> AttachProviderSnapshot {
        AttachProviderSnapshot {
            view_revision: AttachViewRevision(1),
            event_sequence: AttachDeltaSequence(0),
            scene: AttachScene {
                session_id: Uuid::nil(),
                focus: bmux_attach_layout_protocol::AttachFocusTarget::None,
                surfaces: Vec::new(),
            },
            streams: Vec::new(),
            resume: AttachResumeState {
                view_revision: AttachViewRevision(1),
                ..AttachResumeState::default()
            },
        }
    }

    #[test]
    fn rejected_delta_preserves_validated_resume_and_scene() {
        let snapshot = snapshot();
        let mut state = AttachState::from_snapshot(&snapshot).unwrap();
        let before = state.resume_state();
        let delta = AttachProviderDelta {
            sequence: AttachDeltaSequence(2),
            base_view_revision: AttachViewRevision(1),
            view_revision: AttachViewRevision(2),
            changes: Vec::new(),
            resume: before.clone(),
        };
        assert!(state.apply_delta(&delta).is_err());
        assert_eq!(state.resume_state(), before);
        assert_eq!(state.scene(), &snapshot.scene);
        assert!(state.focused_stream().is_none());
    }

    /// Frames from a source grid, through output, scrolling, a resize and the alternate screen,
    /// leave the attach state's replica equal to the source.
    #[test]
    fn stream_frames_keep_the_replica_level_with_its_source() {
        use bmux_attach_layout_protocol::{
            AttachLayer, AttachRect, AttachSurface, AttachSurfaceKind,
        };
        use bmux_client::{AttachStreamFrame, AttachStreamId, AttachStreamSnapshot};
        use bmux_terminal_grid::{GridLimits, ReplicaCursor, TerminalGridStream};
        let surface_id = Uuid::new_v4();
        let rect = AttachRect {
            x: 0,
            y: 0,
            w: 20,
            h: 4,
        };
        let mut snapshot = snapshot();
        snapshot.scene.surfaces.push(AttachSurface {
            id: surface_id,
            kind: AttachSurfaceKind::Pane,
            layer: AttachLayer::Pane,
            z: 0,
            rect,
            content_rect: rect,
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: Some(surface_id),
        });
        let cursor = |offset| AttachStreamCursor {
            stream_id: AttachStreamId::new("stream").unwrap(),
            surface_id,
            generation: 1,
            offset,
        };
        let mut source = TerminalGridStream::new(20, 4, GridLimits::default()).unwrap();
        source.process(b"hello\r\nworld");
        let mut downstream = ReplicaCursor::default();
        let write = |source: &mut TerminalGridStream, downstream: &mut ReplicaCursor| {
            let mut frame = Vec::new();
            let base = downstream.epoch;
            let summary = source
                .grid_mut()
                .write_frame_since(*downstream, 0, &mut frame);
            *downstream = summary.next;
            (base, summary, frame)
        };
        let (_, first, frame) = write(&mut source, &mut downstream);
        assert!(first.reset);
        snapshot.streams.push(AttachStreamSnapshot {
            cursor: cursor(first.next.epoch),
            sync_update_active: false,
            frame,
        });
        snapshot.resume.streams.push(cursor(first.next.epoch));
        let mut state = AttachState::from_snapshot(&snapshot).unwrap();
        let mut sequence = 0;
        let mut send = |state: &mut AttachState, source: &mut TerminalGridStream| {
            let (base, summary, frame) = write(source, &mut downstream);
            sequence += 1;
            state.apply_delta(&AttachProviderDelta {
                sequence: AttachDeltaSequence(sequence),
                base_view_revision: AttachViewRevision(1),
                view_revision: AttachViewRevision(1),
                changes: vec![AttachProviderChange::StreamFrame(AttachStreamFrame {
                    cursor: cursor(base),
                    end_offset: summary.next.epoch,
                    reset: summary.reset,
                    sync_update_active: source.grid().synchronized_update(),
                    frame,
                })],
                resume: AttachResumeState {
                    view_revision: AttachViewRevision(1),
                    event_sequence: AttachDeltaSequence(sequence),
                    streams: vec![cursor(summary.next.epoch)],
                    provider_token: Vec::new(),
                },
            })
        };
        let level = |state: &mut AttachState, source: &TerminalGridStream| {
            let replica = state.buffers_mut()[&surface_id].terminal_grid.grid();
            assert_eq!(replica.width(), source.grid().width());
            assert_eq!(
                bmux_terminal_grid::visible_text(replica, 0, replica.height()),
                bmux_terminal_grid::visible_text(source.grid(), 0, source.grid().height())
            );
            assert_eq!(replica.protocol_state(), source.grid().protocol_state());
        };
        level(&mut state, &source);
        for step in [
            &b"\r\none\r\ntwo\r\nthree"[..],
            b"\x1b[?1049h\x1b[?1002h\x1b[?2026hfull screen",
            b"\x1b[?2026l\x1b[?1049l",
        ] {
            source.process(step);
            send(&mut state, &mut source).unwrap();
            level(&mut state, &source);
        }
        source.resize(30, 6).unwrap();
        source.process(b"wider");
        send(&mut state, &mut source).unwrap();
        level(&mut state, &source);
    }

    #[test]
    fn missing_surface_fails_before_parser_allocation() {
        assert!(render_buffer(&snapshot().scene, Uuid::new_v4()).is_err());
    }
}
