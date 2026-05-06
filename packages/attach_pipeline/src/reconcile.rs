use crate::render::{DamageCoalescingPolicy, DamageRect, FrameDamage, visible_scene_pane_ids};
use crate::types::{PaneRect, PaneRenderBuffer};
use bmux_attach_layout_protocol::{AttachScene, AttachSurface, AttachSurfaceKind};
use bmux_attach_pipeline_models::{AttachChunkApplyOutcome, AttachOutputChunkMeta};
use bmux_client::AttachLayoutState;
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

#[must_use]
pub fn attach_scene_damage_between(
    previous: &AttachScene,
    next: &AttachScene,
    policy: DamageCoalescingPolicy,
) -> FrameDamage {
    if previous == next {
        return FrameDamage::default();
    }

    let mut absolute_damage = Vec::new();
    let previous_surfaces = previous
        .surfaces
        .iter()
        .map(|surface| (surface.id, surface))
        .collect::<BTreeMap<_, _>>();
    let next_surfaces = next
        .surfaces
        .iter()
        .map(|surface| (surface.id, surface))
        .collect::<BTreeMap<_, _>>();

    for (surface_id, previous_surface) in &previous_surfaces {
        match next_surfaces.get(surface_id) {
            Some(next_surface) if *previous_surface == *next_surface => {}
            Some(next_surface) => {
                absolute_damage.push(surface_outer_rect(previous_surface));
                absolute_damage.push(surface_outer_rect(next_surface));
            }
            None => absolute_damage.push(surface_outer_rect(previous_surface)),
        }
    }
    for (surface_id, next_surface) in &next_surfaces {
        if !previous_surfaces.contains_key(surface_id) {
            absolute_damage.push(surface_outer_rect(next_surface));
        }
    }

    attach_scene_damage_for_absolute_rects(next, &absolute_damage, policy)
}

#[must_use]
pub fn attach_scene_damage_for_absolute_rects(
    scene: &AttachScene,
    absolute_damage: &[DamageRect],
    policy: DamageCoalescingPolicy,
) -> FrameDamage {
    let mut damage = FrameDamage::default();
    for surface in scene
        .surfaces
        .iter()
        .filter(|surface| surface_is_pane(surface))
    {
        let outer = surface_outer_rect(surface);
        let content = surface_content_rect(surface);
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        for rect in absolute_damage {
            if let Some(intersection) = intersect_damage_rect(*rect, outer) {
                damage.mark_extension_surface_rect(
                    surface.id,
                    rect_relative_to(intersection, outer),
                    (outer.w, outer.h),
                    policy,
                );
            }
            if let Some(intersection) = intersect_damage_rect(*rect, content) {
                damage.mark_content_surface_rect(
                    pane_id,
                    rect_relative_to(intersection, content),
                    (content.w, content.h),
                    policy,
                );
            }
        }
    }
    damage
}

const fn surface_is_pane(surface: &AttachSurface) -> bool {
    surface.visible
        && surface.pane_id.is_some()
        && matches!(
            surface.kind,
            AttachSurfaceKind::Pane | AttachSurfaceKind::FloatingPane
        )
}

const fn surface_outer_rect(surface: &AttachSurface) -> DamageRect {
    DamageRect::new(
        surface.rect.x,
        surface.rect.y,
        surface.rect.w,
        surface.rect.h,
    )
}

const fn surface_content_rect(surface: &AttachSurface) -> DamageRect {
    DamageRect::new(
        surface.content_rect.x,
        surface.content_rect.y,
        surface.content_rect.w,
        surface.content_rect.h,
    )
}

fn intersect_damage_rect(left: DamageRect, right: DamageRect) -> Option<DamageRect> {
    let x1 = left.x.max(right.x);
    let y1 = left.y.max(right.y);
    let x2 = left.right().min(right.right());
    let y2 = left.bottom().min(right.bottom());
    if x1 >= x2 || y1 >= y2 {
        None
    } else {
        Some(DamageRect::new(
            x1,
            y1,
            x2.saturating_sub(x1),
            y2.saturating_sub(y1),
        ))
    }
}

const fn rect_relative_to(rect: DamageRect, origin: DamageRect) -> DamageRect {
    DamageRect::new(
        rect.x.saturating_sub(origin.x),
        rect.y.saturating_sub(origin.y),
        rect.w,
        rect.h,
    )
}

pub fn resize_attach_grids_for_scene_with_size(
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
        // Size the structured grid to the scene's authoritative content_rect
        // (the PTY interior), clamped to the viewport. This keeps render
        // dimensions aligned with the PTY sizer and mouse translator — no
        // hardcoded border math here.
        let content_x = surface.content_rect.x.min(cols.saturating_sub(1));
        let content_y = surface.content_rect.y.min(rows.saturating_sub(1));
        let max_w = cols.saturating_sub(content_x);
        let max_h = rows.saturating_sub(content_y);
        let inner_w = surface.content_rect.w.min(max_w).max(1);
        let inner_h = surface.content_rect.h.min(max_h).max(1);
        if inner_w == 0 || inner_h == 0 {
            continue;
        }
        let _ = PaneRect {
            x: content_x,
            y: content_y,
            w: inner_w,
            h: inner_h,
        };
        let buffer = pane_buffers.entry(pane_id).or_default();
        let previous_grid_size = (
            buffer.terminal_grid.grid().width(),
            buffer.terminal_grid.grid().height(),
        );
        let _ = buffer.terminal_grid.resize(inner_w, inner_h);
        let next_grid_size = (
            buffer.terminal_grid.grid().width(),
            buffer.terminal_grid.grid().height(),
        );
        if next_grid_size != previous_grid_size {
            buffer.prev_rows.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_attach_layout_protocol::{AttachFocusTarget, AttachLayer, AttachRect};

    fn pane_surface(
        id: Uuid,
        pane_id: Uuid,
        origin_x: u16,
        origin_y: u16,
        width: u16,
        height: u16,
        z_index: i32,
    ) -> AttachSurface {
        AttachSurface {
            id,
            kind: AttachSurfaceKind::Pane,
            layer: AttachLayer::Pane,
            z: z_index,
            rect: AttachRect {
                x: origin_x,
                y: origin_y,
                w: width,
                h: height,
            },
            content_rect: AttachRect {
                x: origin_x,
                y: origin_y,
                w: width,
                h: height,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: Some(pane_id),
        }
    }

    fn scene(surfaces: Vec<AttachSurface>) -> AttachScene {
        AttachScene {
            session_id: Uuid::from_u128(100),
            focus: AttachFocusTarget::Pane {
                pane_id: surfaces[0].pane_id.expect("surface has pane"),
            },
            surfaces,
        }
    }

    #[test]
    fn resize_attach_grids_clears_row_cache_on_dimension_change() {
        let pane_id = Uuid::from_u128(1);
        let test_scene = scene(vec![pane_surface(
            Uuid::from_u128(2),
            pane_id,
            0,
            0,
            10,
            3,
            0,
        )]);
        let mut pane_buffers: BTreeMap<Uuid, PaneRenderBuffer> = BTreeMap::new();
        let buffer = pane_buffers.entry(pane_id).or_default();
        buffer.prev_rows.push("cached".to_string());

        resize_attach_grids_for_scene_with_size(&mut pane_buffers, &test_scene, 10, 3);

        assert!(pane_buffers[&pane_id].prev_rows.is_empty());
    }

    #[test]
    fn attach_scene_damage_between_marks_old_and_new_surface_regions() {
        let background = Uuid::from_u128(1);
        let floating = Uuid::from_u128(2);
        let background_surface = Uuid::from_u128(10);
        let floating_surface = Uuid::from_u128(20);
        let previous = scene(vec![
            pane_surface(background_surface, background, 0, 0, 20, 10, 0),
            pane_surface(floating_surface, floating, 2, 2, 4, 3, 1),
        ]);
        let next = scene(vec![
            pane_surface(background_surface, background, 0, 0, 20, 10, 0),
            pane_surface(floating_surface, floating, 8, 2, 4, 3, 1),
        ]);

        let damage =
            attach_scene_damage_between(&previous, &next, DamageCoalescingPolicy::default());

        assert_eq!(
            damage.content_surface_rects(background),
            &[DamageRect::new(2, 2, 4, 3), DamageRect::new(8, 2, 4, 3)]
        );
        assert!(damage.extension_surface_damaged(floating_surface, floating));
    }
}
