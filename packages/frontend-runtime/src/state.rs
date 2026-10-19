//! Provider-neutral parser state shared by terminal and native frontends.
//!
//! Stream bytes update the existing terminal grids; this module neither executes
//! PTYs nor constructs an alternate layout, image registry, or hit map.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use bmux_attach_layout_protocol::AttachScene;
use bmux_attach_pipeline::PaneRenderBuffer;
use bmux_attach_pipeline::render::append_pane_output;
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

/// Build grids from complete provider repair bytes. Shared with the CLI adapter.
pub fn pane_buffers_from_snapshot(
    snapshot: &AttachProviderSnapshot,
) -> Result<BTreeMap<Uuid, PaneRenderBuffer>> {
    let mut buffers = BTreeMap::new();
    for stream in &snapshot.streams {
        let mut buffer = render_buffer(&snapshot.scene, stream.cursor.surface_id)?;
        append_pane_output(&mut buffer, &stream.snapshot);
        buffers.insert(stream.cursor.surface_id, buffer);
    }
    Ok(buffers)
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
            AttachProviderChange::StreamAppend { cursor, bytes, .. } => {
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    buffers.entry(cursor.surface_id)
                {
                    entry.insert(render_buffer(scene, cursor.surface_id)?);
                }
                if let Some(buffer) = buffers.get_mut(&cursor.surface_id) {
                    append_pane_output(buffer, bytes);
                }
            }
            AttachProviderChange::StreamRepair(snapshot) => {
                let mut buffer = render_buffer(scene, snapshot.cursor.surface_id)?;
                append_pane_output(&mut buffer, &snapshot.snapshot);
                buffers.insert(snapshot.cursor.surface_id, buffer);
            }
            AttachProviderChange::Scene(replacement) => {
                for surface in &replacement.surfaces {
                    if let Some(buffer) = buffers.get_mut(&surface.id) {
                        buffer
                            .terminal_grid
                            .resize(surface.content_rect.w.max(1), surface.content_rect.h.max(1))
                            .map_err(|error| {
                                anyhow::anyhow!(
                                    "attach provider replacement surface grid is invalid: {error}"
                                )
                            })?;
                    }
                }
            }
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

    #[test]
    fn missing_surface_fails_before_parser_allocation() {
        assert!(render_buffer(&snapshot().scene, Uuid::new_v4()).is_err());
    }
}
