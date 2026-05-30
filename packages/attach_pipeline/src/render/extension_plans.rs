use crate::types::{ExtensionLayerSnapshotCacheEntry, PaneRect, PaneRenderBuffer};
use bmux_plugin::{
    AttachRenderExtension, ExtensionRect, RenderDamage, RenderExtensionContext,
    RenderExtensionLayer, RenderLayerItem, RenderOp, RenderUnderCell, TerminalRenderCapabilities,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

use super::extension_retained::{
    ExtensionLayerDiffPlan, ExtensionRetainedLayerSnapshot, build_extension_layer_diff_plan,
    load_retained_layer_snapshot, retained_layer_cache_key,
    retained_scene_item_damage_for_render_damage, retained_scene_items_to_render_items,
    retained_snapshot_from_plugin_scene,
};
use super::{
    DamageCoalescingPolicy, DamageRect, FrameDamage, RenderOpsOutputPlan,
    build_render_ops_output_plan, coalesce_render_damage, frame_rects_to_render_damage,
};

fn merge_render_damage(
    lhs: RenderDamage,
    rhs: RenderDamage,
    surface_rect: ExtensionRect,
    policy: DamageCoalescingPolicy,
) -> RenderDamage {
    match (lhs, rhs) {
        (RenderDamage::FullSurface, _) | (_, RenderDamage::FullSurface) => {
            RenderDamage::FullSurface
        }
        (RenderDamage::None, damage) | (damage, RenderDamage::None) => damage,
        (RenderDamage::Regions(mut lhs), RenderDamage::Regions(rhs)) => {
            lhs.extend(rhs);
            coalesce_render_damage(RenderDamage::Regions(lhs), surface_rect, policy)
        }
    }
}

fn extension_own_render_damage_for_frame(
    ext: &dyn AttachRenderExtension,
    surface_id: Uuid,
    surface_rect: ExtensionRect,
    layer: RenderExtensionLayer,
    frame_damage: &FrameDamage,
    policy: DamageCoalescingPolicy,
) -> RenderDamage {
    let extension_rect_damage = frame_damage.extension_surface_rects(surface_id);
    if frame_damage.is_full_frame() || frame_damage.extension_surfaces.contains(&surface_id) {
        return RenderDamage::FullSurface;
    }
    if !extension_rect_damage.is_empty() {
        return coalesce_render_damage(
            frame_rects_to_render_damage(extension_rect_damage, surface_rect),
            surface_rect,
            policy,
        );
    }
    coalesce_render_damage(
        ext.surface_layer_damage(surface_id, &surface_rect, layer),
        surface_rect,
        policy,
    )
}

// Compatibility boundary for existing damage-oriented extension APIs.
//
// Long-term retained-scene rendering should make plugins publish current visual
// state and let the attach renderer diff old/new retained items. Until that is
// wired in, `ExtensionLayerSnapshot` keeps the existing contracts explicit:
// `own_damage` is extension-reported update damage, `render_damage` adds host
// replay requirements such as content-damage redraws, and previous-snapshot
// cleanup is reserved for stale/removal/geometry/no-own-damage fallback cases.
// A revision change alone must not imply stale cleanup; animated decorations
// change revisions constantly and should use precise own damage when available.
#[derive(Clone)]
pub(super) struct ExtensionLayerSnapshot {
    pub(super) extension: Arc<dyn AttachRenderExtension>,
    pub(super) surface_id: Uuid,
    pub(super) pane_id: Uuid,
    pub(super) surface_rect: ExtensionRect,
    pub(super) layer: RenderExtensionLayer,
    pub(super) own_damage: RenderDamage,
    pub(super) render_damage: RenderDamage,
    pub(super) revision: Option<u64>,
}

impl ExtensionLayerSnapshot {
    pub(super) fn build(
        extension: &Arc<dyn AttachRenderExtension>,
        surface_id: Uuid,
        pane_id: Uuid,
        surface_rect: ExtensionRect,
        layer: RenderExtensionLayer,
        frame_damage: &FrameDamage,
        policy: DamageCoalescingPolicy,
    ) -> Self {
        let ext = extension.as_ref();
        let own_damage = extension_own_render_damage_for_frame(
            ext,
            surface_id,
            surface_rect,
            layer,
            frame_damage,
            policy,
        );
        let render_damage = if !own_damage.is_none() {
            own_damage.clone()
        } else if frame_damage.content_surface_damaged(pane_id)
            && ext.redraws_on_content_damage(layer)
        {
            RenderDamage::FullSurface
        } else {
            RenderDamage::None
        };
        let revision = ext.render_layer_revision(surface_id, layer);
        Self {
            extension: extension.clone(),
            surface_id,
            pane_id,
            surface_rect,
            layer,
            own_damage,
            render_damage,
            revision,
        }
    }

    pub(super) fn cache_key(&self, capabilities: TerminalRenderCapabilities) -> (String, Uuid) {
        (
            format!(
                "{}::{:?}::caps={}",
                self.extension.name(),
                self.layer,
                capabilities.cache_key()
            ),
            self.surface_id,
        )
    }
}

pub(super) fn extension_layer_snapshots_for_surface(
    render_extensions: &[Arc<dyn AttachRenderExtension>],
    surface_id: Uuid,
    pane_id: Uuid,
    surface_rect: ExtensionRect,
    layer: RenderExtensionLayer,
    frame_damage: &FrameDamage,
    policy: DamageCoalescingPolicy,
) -> Vec<ExtensionLayerSnapshot> {
    render_extensions
        .iter()
        .map(|extension| {
            ExtensionLayerSnapshot::build(
                extension,
                surface_id,
                pane_id,
                surface_rect,
                layer,
                frame_damage,
                policy,
            )
        })
        .collect()
}

fn extension_snapshot_changed(
    previous: &ExtensionLayerSnapshotCacheEntry,
    current: &ExtensionLayerSnapshot,
) -> bool {
    extension_snapshot_geometry_changed(previous, current)
        || extension_snapshot_revision_changed(previous, current)
}

fn extension_snapshot_geometry_changed(
    previous: &ExtensionLayerSnapshotCacheEntry,
    current: &ExtensionLayerSnapshot,
) -> bool {
    previous.surface_rect != current.surface_rect
}

fn extension_snapshot_revision_changed(
    previous: &ExtensionLayerSnapshotCacheEntry,
    current: &ExtensionLayerSnapshot,
) -> bool {
    previous.revision != current.revision
}

fn extension_snapshot_needs_previous_cleanup(
    previous: &ExtensionLayerSnapshotCacheEntry,
    current: &ExtensionLayerSnapshot,
) -> bool {
    // Same-surface revision changes are normal for animated terminal-cell
    // decorations. When the extension reports precise old/new own damage,
    // trust that instead of layering on a previous full-snapshot cleanup that
    // would clear the whole pane and visibly flicker between animation ticks.
    if extension_snapshot_geometry_changed(previous, current) {
        return true;
    }
    extension_snapshot_revision_changed(previous, current) && current.own_damage.is_none()
}

pub(super) fn apply_previous_extension_snapshot_damage(
    pane_buffer: Option<&PaneRenderBuffer>,
    capabilities: TerminalRenderCapabilities,
    layer_snapshots: &mut [ExtensionLayerSnapshot],
) {
    let Some(buffer) = pane_buffer else {
        return;
    };
    for snapshot in layer_snapshots {
        if !snapshot.render_damage.is_none() {
            continue;
        }
        let key = snapshot.cache_key(capabilities);
        if buffer
            .extension_layer_snapshot_cache
            .get(&key)
            .is_some_and(|previous| extension_snapshot_changed(previous, snapshot))
        {
            snapshot.render_damage = RenderDamage::FullSurface;
        }
    }
}

pub(super) fn previous_extension_snapshot_cleanup_damage(
    pane_buffer: Option<&PaneRenderBuffer>,
    surface_id: Uuid,
    layer: RenderExtensionLayer,
    surface_rect: ExtensionRect,
    policy: DamageCoalescingPolicy,
    capabilities: TerminalRenderCapabilities,
    layer_snapshots: &[ExtensionLayerSnapshot],
) -> RenderDamage {
    let Some(buffer) = pane_buffer else {
        return RenderDamage::None;
    };
    let current_keys = layer_snapshots
        .iter()
        .map(|snapshot| snapshot.cache_key(capabilities))
        .collect::<BTreeSet<_>>();
    let mut rects = Vec::new();
    for (key, previous) in &buffer.extension_layer_snapshot_cache {
        if previous.surface_id != surface_id || previous.layer != layer {
            continue;
        }
        let stale = !current_keys.contains(key)
            || layer_snapshots
                .iter()
                .find(|snapshot| snapshot.cache_key(capabilities) == *key)
                .is_some_and(|snapshot| {
                    extension_snapshot_needs_previous_cleanup(previous, snapshot)
                });
        if !stale {
            continue;
        }
        match &previous.full_snapshot_damage {
            RenderDamage::None => {}
            RenderDamage::FullSurface => return RenderDamage::FullSurface,
            RenderDamage::Regions(regions) => rects.extend(regions.iter().copied()),
        }
    }
    coalesce_render_damage(RenderDamage::Regions(rects), surface_rect, policy)
}

pub(super) fn commit_extension_layer_snapshots_for_surface(
    pane_buffers: &mut BTreeMap<Uuid, PaneRenderBuffer>,
    capabilities: TerminalRenderCapabilities,
    pane_id: Uuid,
    surface_id: Uuid,
    layer: RenderExtensionLayer,
    layer_snapshots: &[ExtensionLayerSnapshot],
) {
    let Some(buffer) = pane_buffers.get_mut(&pane_id) else {
        return;
    };
    let current_keys = layer_snapshots
        .iter()
        .map(|snapshot| snapshot.cache_key(capabilities))
        .collect::<BTreeSet<_>>();
    buffer.extension_layer_snapshot_cache.retain(|key, entry| {
        entry.surface_id != surface_id || entry.layer != layer || current_keys.contains(key)
    });
    for snapshot in layer_snapshots {
        let key = snapshot.cache_key(capabilities);
        let had_previous = buffer.extension_layer_snapshot_cache.contains_key(&key);
        if snapshot.render_damage.is_none() && !had_previous {
            continue;
        }
        buffer.extension_layer_snapshot_cache.insert(
            key,
            ExtensionLayerSnapshotCacheEntry {
                surface_id: snapshot.surface_id,
                surface_rect: snapshot.surface_rect,
                layer: snapshot.layer,
                emitted_damage: snapshot.render_damage.clone(),
                full_snapshot_damage: RenderDamage::FullSurface,
                revision: snapshot.revision,
            },
        );
    }
}

pub(super) struct BeforeContentSurfaceOutputPlan {
    pub(super) plans: Vec<BeforeContentExtensionOutputPlan>,
    pub(super) retained_extension_names: BTreeSet<String>,
    pub(super) damage_rects: Vec<DamageRect>,
}

pub(super) fn build_before_content_surface_output_plan(
    layer_snapshots: &[ExtensionLayerSnapshot],
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> BeforeContentSurfaceOutputPlan {
    let mut plans = Vec::new();
    let mut damage_rects = Vec::new();
    for snapshot in layer_snapshots {
        let Some(plan) = build_before_content_extension_output_plan(
            snapshot,
            content,
            policy,
            render_context,
            pane_buffers,
        ) else {
            continue;
        };
        damage_rects.extend(plan.damage_rects.iter().copied());
        plans.push(plan);
    }
    let retained_extension_names = plans
        .iter()
        .filter(|plan| {
            matches!(
                plan.action,
                BeforeContentExtensionOutputAction::RetainedScene { .. }
            )
        })
        .map(|plan| plan.snapshot.extension.name().to_string())
        .collect();
    BeforeContentSurfaceOutputPlan {
        plans,
        retained_extension_names,
        damage_rects,
    }
}

#[derive(Clone)]
pub(super) struct BeforeContentExtensionOutputPlan {
    pub(super) snapshot: ExtensionLayerSnapshot,
    pub(super) cache_key: Option<(String, Uuid)>,
    pub(super) damage_rects: Vec<DamageRect>,
    pub(super) action: BeforeContentExtensionOutputAction,
}

#[derive(Clone)]
pub(super) enum BeforeContentExtensionOutputAction {
    RetainedScene {
        diff_plan: Box<ExtensionLayerDiffPlan>,
        snapshot: ExtensionRetainedLayerSnapshot,
        output_damage: RenderDamage,
        output_items: Vec<RenderLayerItem>,
    },
    RenderItems {
        items: Vec<RenderLayerItem>,
    },
    LayerCells {
        cells: Vec<(u16, u16, RenderUnderCell)>,
    },
    RenderOps {
        ops: Vec<RenderOp>,
    },
    NoOutput,
}

pub(super) fn build_before_content_extension_output_plan(
    snapshot: &ExtensionLayerSnapshot,
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> Option<BeforeContentExtensionOutputPlan> {
    if let Some(plan) = build_retained_before_content_extension_output_plan(
        snapshot,
        content,
        policy,
        render_context,
        pane_buffers,
    ) {
        return Some(plan);
    }

    let damage = snapshot.render_damage.clone();
    if damage.is_none() {
        return None;
    }
    let action = before_content_extension_output_action(snapshot, &damage, render_context);
    Some(BeforeContentExtensionOutputPlan {
        snapshot: snapshot.clone(),
        cache_key: None,
        damage_rects: before_content_damage_rects(&damage, content),
        action,
    })
}

fn build_retained_before_content_extension_output_plan(
    snapshot: &ExtensionLayerSnapshot,
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> Option<BeforeContentExtensionOutputPlan> {
    let retained_cache_key = retained_layer_cache_key(
        snapshot.extension.name(),
        snapshot.surface_id,
        snapshot.layer,
        render_context.capabilities,
    );
    let scene = snapshot.extension.render_layer_scene_with_context(
        snapshot.surface_id,
        &snapshot.surface_rect,
        snapshot.layer,
        render_context,
    )?;
    let retained_snapshot = retained_snapshot_from_plugin_scene(snapshot.surface_rect, &scene);
    let previous_snapshot =
        load_retained_layer_snapshot(pane_buffers.get(&snapshot.pane_id), &retained_cache_key);
    let diff_plan = build_extension_layer_diff_plan(
        previous_snapshot.as_ref(),
        Some(&retained_snapshot),
        content,
        policy,
    );
    let retained_replay_damage = if snapshot.own_damage.is_none() {
        retained_scene_item_damage_for_render_damage(
            &retained_snapshot.items,
            &snapshot.render_damage,
            snapshot.surface_rect,
            policy,
        )
    } else {
        RenderDamage::None
    };
    let output_damage = merge_render_damage(
        merge_render_damage(
            diff_plan.update_damage.clone(),
            diff_plan.stale_cleanup_damage.clone(),
            snapshot.surface_rect,
            policy,
        ),
        retained_replay_damage,
        snapshot.surface_rect,
        policy,
    );
    let output_items = retained_scene_items_to_render_items(&diff_plan.output_items);
    Some(BeforeContentExtensionOutputPlan {
        snapshot: snapshot.clone(),
        cache_key: Some(retained_cache_key),
        damage_rects: before_content_damage_rects(&output_damage, content),
        action: BeforeContentExtensionOutputAction::RetainedScene {
            diff_plan: Box::new(diff_plan),
            snapshot: retained_snapshot,
            output_damage,
            output_items,
        },
    })
}

fn before_content_extension_output_action(
    snapshot: &ExtensionLayerSnapshot,
    damage: &RenderDamage,
    render_context: &RenderExtensionContext,
) -> BeforeContentExtensionOutputAction {
    if let Some(items) = snapshot.extension.render_layer_items_with_context(
        snapshot.surface_id,
        &snapshot.surface_rect,
        damage,
        snapshot.layer,
        render_context,
    ) {
        return BeforeContentExtensionOutputAction::RenderItems { items };
    }
    if let Some(cells) = snapshot.extension.render_before_content_cells_with_context(
        snapshot.surface_id,
        &snapshot.surface_rect,
        damage,
        render_context,
    ) {
        return BeforeContentExtensionOutputAction::LayerCells { cells };
    }
    if let Some(ops) = snapshot.extension.render_layer_ops_with_context(
        snapshot.surface_id,
        &snapshot.surface_rect,
        damage,
        snapshot.layer,
        render_context,
    ) {
        return BeforeContentExtensionOutputAction::RenderOps { ops };
    }
    BeforeContentExtensionOutputAction::NoOutput
}

fn before_content_damage_rects(damage: &RenderDamage, content: PaneRect) -> Vec<DamageRect> {
    match damage {
        RenderDamage::FullSurface => vec![DamageRect::new(0, 0, content.w, content.h)],
        RenderDamage::Regions(regions) => regions
            .iter()
            .filter_map(|region| {
                let x1 = region.x.max(content.x);
                let y1 = region.y.max(content.y);
                let x2 = region.right().min(content.x.saturating_add(content.w));
                let y2 = region.bottom().min(content.y.saturating_add(content.h));
                (x1 < x2 && y1 < y2).then_some(DamageRect::new(
                    x1.saturating_sub(content.x),
                    y1.saturating_sub(content.y),
                    x2.saturating_sub(x1),
                    y2.saturating_sub(y1),
                ))
            })
            .collect(),
        RenderDamage::None => Vec::new(),
    }
}

pub(super) struct AfterContentSurfaceOutputPlan {
    pub(super) plans: Vec<AfterContentExtensionOutputPlan>,
    pub(super) retained_snapshot_keys: BTreeSet<(String, Uuid)>,
    pub(super) retained_extension_names: BTreeSet<String>,
    pub(super) retained_cleanup_damage: RenderDamage,
}

#[derive(Clone)]
pub(super) struct AfterContentExtensionOutputPlan {
    pub(super) surface_index: usize,
    pub(super) snapshot: ExtensionLayerSnapshot,
    pub(super) cache_key: (String, Uuid),
    pub(super) action: AfterContentExtensionOutputAction,
}

#[derive(Clone)]
pub(super) enum AfterContentExtensionOutputAction {
    RetainedScene {
        diff_plan: Box<ExtensionLayerDiffPlan>,
        snapshot: ExtensionRetainedLayerSnapshot,
        output_damage: RenderDamage,
        output_items: Vec<RenderLayerItem>,
    },
    CachedReplay {
        bytes: Vec<u8>,
    },
    RenderItems {
        items: Vec<RenderLayerItem>,
    },
    RenderOps {
        output_plan: RenderOpsOutputPlan,
    },
    Imperative,
}

pub(super) fn build_after_content_surface_output_plan(
    surface_index: usize,
    layer_snapshots: &[ExtensionLayerSnapshot],
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> AfterContentSurfaceOutputPlan {
    let mut retained_snapshot_keys = BTreeSet::new();
    let mut retained_extension_names = BTreeSet::new();
    let mut retained_cleanup_regions = Vec::new();
    let plans = layer_snapshots
        .iter()
        .filter_map(|snapshot| {
            let plan = build_after_content_extension_output_plan(
                surface_index,
                snapshot,
                content,
                policy,
                render_context,
                pane_buffers,
            )?;
            if let AfterContentExtensionOutputAction::RetainedScene { diff_plan, .. } = &plan.action
            {
                retained_snapshot_keys.insert(snapshot.cache_key(render_context.capabilities));
                retained_extension_names.insert(snapshot.extension.name().to_string());
                match &diff_plan.stale_cleanup_damage {
                    RenderDamage::None => {}
                    RenderDamage::FullSurface => {
                        retained_cleanup_regions.clear();
                        retained_cleanup_regions.push(snapshot.surface_rect);
                    }
                    RenderDamage::Regions(regions) => {
                        retained_cleanup_regions.extend(regions.iter().copied());
                    }
                }
            }
            Some(plan)
        })
        .collect();
    let retained_cleanup_damage = coalesce_render_damage(
        RenderDamage::Regions(retained_cleanup_regions),
        layer_snapshots.first().map_or_else(
            || ExtensionRect::new(0, 0, 0, 0),
            |snapshot| snapshot.surface_rect,
        ),
        policy,
    );
    AfterContentSurfaceOutputPlan {
        plans,
        retained_snapshot_keys,
        retained_extension_names,
        retained_cleanup_damage,
    }
}

fn build_retained_after_content_extension_output_plan(
    surface_index: usize,
    snapshot: &ExtensionLayerSnapshot,
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> Option<AfterContentExtensionOutputPlan> {
    let retained_cache_key = retained_layer_cache_key(
        snapshot.extension.name(),
        snapshot.surface_id,
        snapshot.layer,
        render_context.capabilities,
    );
    let scene = snapshot.extension.render_layer_scene_with_context(
        snapshot.surface_id,
        &snapshot.surface_rect,
        snapshot.layer,
        render_context,
    )?;
    let retained_snapshot = retained_snapshot_from_plugin_scene(snapshot.surface_rect, &scene);
    let previous_snapshot =
        load_retained_layer_snapshot(pane_buffers.get(&snapshot.pane_id), &retained_cache_key);
    let diff_plan = build_extension_layer_diff_plan(
        previous_snapshot.as_ref(),
        Some(&retained_snapshot),
        content,
        policy,
    );
    let retained_replay_damage = if snapshot.own_damage.is_none() {
        retained_scene_item_damage_for_render_damage(
            &retained_snapshot.items,
            &snapshot.render_damage,
            snapshot.surface_rect,
            policy,
        )
    } else {
        RenderDamage::None
    };
    let output_damage = merge_render_damage(
        diff_plan.update_damage.clone(),
        retained_replay_damage,
        snapshot.surface_rect,
        policy,
    );
    let output_items = retained_scene_items_to_render_items(&diff_plan.output_items);
    Some(AfterContentExtensionOutputPlan {
        surface_index,
        snapshot: snapshot.clone(),
        cache_key: retained_cache_key,
        action: AfterContentExtensionOutputAction::RetainedScene {
            diff_plan: Box::new(diff_plan),
            snapshot: retained_snapshot,
            output_damage,
            output_items,
        },
    })
}

pub(super) fn build_after_content_extension_output_plan(
    surface_index: usize,
    snapshot: &ExtensionLayerSnapshot,
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> Option<AfterContentExtensionOutputPlan> {
    if let Some(plan) = build_retained_after_content_extension_output_plan(
        surface_index,
        snapshot,
        content,
        policy,
        render_context,
        pane_buffers,
    ) {
        return Some(plan);
    }

    let damage = snapshot.render_damage.clone();
    if damage.is_none() {
        return None;
    }
    let cache_key = snapshot.cache_key(render_context.capabilities);
    let action = if let Some(revision) = snapshot.revision
        && let Some(entry) = pane_buffers
            .get(&snapshot.pane_id)
            .and_then(|buffer| buffer.extension_render_cache.get(&cache_key))
        && entry.surface_rect == snapshot.surface_rect
        && entry.damage == damage
        && entry.revision == revision
    {
        AfterContentExtensionOutputAction::CachedReplay {
            bytes: entry.bytes.clone(),
        }
    } else if let Some(items) = snapshot.extension.render_layer_items_with_context(
        snapshot.surface_id,
        &snapshot.surface_rect,
        &damage,
        snapshot.layer,
        render_context,
    ) {
        AfterContentExtensionOutputAction::RenderItems { items }
    } else if let Some(ops) = snapshot.extension.render_layer_ops_with_context(
        snapshot.surface_id,
        &snapshot.surface_rect,
        &damage,
        snapshot.layer,
        render_context,
    ) {
        if ops.is_empty() {
            return None;
        }
        AfterContentExtensionOutputAction::RenderOps {
            output_plan: build_render_ops_output_plan(snapshot.surface_rect, &damage, &ops),
        }
    } else {
        AfterContentExtensionOutputAction::Imperative
    };
    Some(AfterContentExtensionOutputPlan {
        surface_index,
        snapshot: snapshot.clone(),
        cache_key,
        action,
    })
}
