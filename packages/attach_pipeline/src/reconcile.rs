use crate::render::visible_scene_pane_ids;
use crate::types::{PaneRect, PaneRenderBuffer};
use bmux_attach_pipeline_models::{AttachChunkApplyOutcome, AttachOutputChunkMeta};
use bmux_client::AttachLayoutState;
use bmux_ipc::AttachScene;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub fn apply_attach_output_chunk_with(
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    pane_id: Uuid,
    bytes: &[u8],
    meta: AttachOutputChunkMeta,
    mut apply_bytes: impl FnMut(&mut PaneRenderBuffer, &[u8]) -> bool,
) -> AttachChunkApplyOutcome {
    {
        let buffer = pane_buffers.entry(pane_id).or_default();

        if meta.stream_end < meta.stream_start {
            return AttachChunkApplyOutcome::Desync;
        }

        if meta.stream_gap {
            return AttachChunkApplyOutcome::Desync;
        }

        if let Some(expected) = buffer.expected_stream_start {
            if meta.stream_end <= expected {
                return AttachChunkApplyOutcome::Stale;
            }
            if meta.stream_start != expected {
                return AttachChunkApplyOutcome::Desync;
            }
        }
    }

    let buffer = pane_buffers.entry(pane_id).or_default();
    let had_data = apply_bytes(buffer, bytes);
    buffer.sync_update_in_progress = meta.sync_update_active;
    buffer.expected_stream_start = Some(meta.stream_end);

    AttachChunkApplyOutcome::Applied { had_data }
}

#[must_use]
pub fn attach_scene_visible_pane_id_set(scene: &AttachScene) -> BTreeSet<Uuid> {
    visible_scene_pane_ids(scene).into_iter().collect()
}

#[must_use]
pub fn attach_scene_revealed_pane_ids(
    previous: &AttachScene,
    next: &AttachScene,
) -> BTreeSet<Uuid> {
    let previous_visible = attach_scene_visible_pane_id_set(previous);
    let next_visible = attach_scene_visible_pane_id_set(next);
    next_visible
        .difference(&previous_visible)
        .copied()
        .collect()
}

#[must_use]
pub fn attach_layout_pane_id_set(layout_state: &AttachLayoutState) -> BTreeSet<Uuid> {
    layout_state.panes.iter().map(|pane| pane.id).collect()
}

#[must_use]
pub fn attach_layout_requires_snapshot_hydration(
    previous: &AttachLayoutState,
    next: &AttachLayoutState,
) -> bool {
    if previous.session_id != next.session_id {
        return true;
    }
    if previous.layout_root != next.layout_root {
        return true;
    }
    attach_layout_pane_id_set(previous) != attach_layout_pane_id_set(next)
}

pub fn resize_attach_parsers_for_scene_with_size(
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    scene: &AttachScene,
    cols: u16,
    rows: u16,
) {
    if cols == 0 || rows <= 1 {
        return;
    }

    for surface in &scene.surfaces {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        let rect = PaneRect {
            x: surface.rect.x.min(cols.saturating_sub(1)),
            y: surface.rect.y.min(rows.saturating_sub(1)),
            w: surface.rect.w.min(cols),
            h: surface
                .rect
                .h
                .min(rows.saturating_sub(surface.rect.y.min(rows.saturating_sub(1)))),
        };
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let inner_w = rect.w.saturating_sub(2).max(1);
        let inner_h = rect.h.saturating_sub(2).max(1);
        let buffer = pane_buffers.entry(pane_id).or_default();
        buffer.parser.screen_mut().set_size(inner_h, inner_w);
    }
}
