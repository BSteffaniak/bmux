use crate::types::{PaneRect, PaneRenderBuffer};
use bmux_plugin::{
    AttachRenderExtension, ExtensionRect, RenderDamage, RenderExtensionContext,
    RenderExtensionLayer, RenderLayerItem,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

use super::extension_retained::{
    ExtensionLayerDiffPlan, ExtensionRetainedLayerSnapshot, build_extension_layer_diff_plan,
    load_retained_layer_snapshot, retained_layer_cache_key,
    retained_scene_item_damage_for_render_damage, retained_scene_items_for_render_damage,
    retained_scene_items_to_render_items, retained_snapshot_from_plugin_scene,
};
use super::{DamageCoalescingPolicy, DamageRect, FrameDamage, coalesce_render_damage};
use bmux_plugin::RenderLayerScene as PluginRenderLayerScene;

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

/// One extension's layer on one surface this frame: whose scene to plan, and where pane content
/// repaints under it (its items there are painted again, content replay).
#[derive(Clone)]
pub(super) struct ExtensionLayerSnapshot {
    pub(super) extension: Arc<dyn AttachRenderExtension>,
    pub(super) surface_id: Uuid,
    pub(super) pane_id: Uuid,
    pub(super) surface_rect: ExtensionRect,
    pub(super) layer: RenderExtensionLayer,
    pub(super) content_replay_damage: RenderDamage,
}

impl ExtensionLayerSnapshot {
    pub(super) fn build(
        extension: &Arc<dyn AttachRenderExtension>,
        surface_id: Uuid,
        pane_id: Uuid,
        surface_rect: ExtensionRect,
        layer: RenderExtensionLayer,
        frame_damage: &FrameDamage,
    ) -> Self {
        let content_replay_damage = if frame_damage.content_surface_damaged(pane_id) {
            RenderDamage::FullSurface
        } else {
            RenderDamage::None
        };
        Self {
            extension: extension.clone(),
            surface_id,
            pane_id,
            surface_rect,
            layer,
            content_replay_damage,
        }
    }
}

pub(super) fn extension_layer_snapshots_for_surface(
    render_extensions: &[Arc<dyn AttachRenderExtension>],
    surface_id: Uuid,
    pane_id: Uuid,
    surface_rect: ExtensionRect,
    layer: RenderExtensionLayer,
    frame_damage: &FrameDamage,
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
            )
        })
        .collect()
}

pub(super) struct BeforeContentSurfaceOutputPlan {
    pub(super) plans: Vec<BeforeContentExtensionOutputPlan>,
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
    BeforeContentSurfaceOutputPlan {
        plans,
        damage_rects,
    }
}

#[derive(Clone)]
pub(super) struct BeforeContentExtensionOutputPlan {
    pub(super) snapshot: ExtensionLayerSnapshot,
    pub(super) cache_key: (String, Uuid),
    pub(super) damage_rects: Vec<DamageRect>,
    pub(super) output: RetainedLayerOutput,
}

/// What one extension layer's retained scene paints this frame.
#[derive(Clone)]
pub(super) struct RetainedLayerOutput {
    pub(super) diff_plan: Box<ExtensionLayerDiffPlan>,
    pub(super) snapshot: Arc<ExtensionRetainedLayerSnapshot>,
    /// The extension's scene `snapshot` was built from.
    pub(super) scene: Arc<PluginRenderLayerScene>,
    /// `snapshot` is the one already retained: there is nothing to commit.
    pub(super) reused: bool,
    pub(super) content_replay_damage: RenderDamage,
    pub(super) output_damage: RenderDamage,
    pub(super) output_items: Vec<RenderLayerItem>,
}

/// A retained-scene layer planned against what it emitted last.
struct RetainedLayerPlan {
    cache_key: (String, Uuid),
    scene: Arc<PluginRenderLayerScene>,
    snapshot: Arc<ExtensionRetainedLayerSnapshot>,
    diff_plan: ExtensionLayerDiffPlan,
    /// The extension answered with the very scene emitted last, which is reused as is.
    reused: bool,
    content_replay_damage: RenderDamage,
}

/// Plan the layer of `snapshot` when its extension answers with a retained scene. A scene that is
/// the same allocation as the one last emitted on the same surface rect is unchanged: its
/// converted snapshot is reused and nothing is diffed, leaving only content replay.
fn retained_layer_plan(
    snapshot: &ExtensionLayerSnapshot,
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> Option<RetainedLayerPlan> {
    let cache_key = retained_layer_cache_key(
        snapshot.extension.name(),
        snapshot.surface_id,
        snapshot.layer,
        render_context.capabilities,
    );
    let scene = snapshot.extension.render_layer_scene_shared(
        snapshot.surface_id,
        &snapshot.surface_rect,
        snapshot.layer,
        render_context,
    )?;
    let previous = load_retained_layer_snapshot(pane_buffers.get(&snapshot.pane_id), &cache_key);
    let reusable = previous.filter(|previous| {
        previous.snapshot.surface_rect == snapshot.surface_rect
            && previous
                .scene
                .as_ref()
                .is_some_and(|previous| Arc::ptr_eq(previous, &scene))
    });
    let (retained, diff_plan, reused) = reusable.map_or_else(
        || {
            let retained = Arc::new(retained_snapshot_from_plugin_scene(
                snapshot.surface_rect,
                &scene,
            ));
            let diff_plan = build_extension_layer_diff_plan(
                previous.map(|previous| previous.snapshot.as_ref()),
                Some(&retained),
                content,
                policy,
            );
            (retained, diff_plan, false)
        },
        |previous| {
            let retained = previous.snapshot.clone();
            let diff_plan = ExtensionLayerDiffPlan::reused(retained.items.len());
            (retained, diff_plan, true)
        },
    );
    let content_replay_damage = retained_scene_item_damage_for_render_damage(
        &retained.items,
        &snapshot.content_replay_damage,
        snapshot.surface_rect,
        policy,
    );
    Some(RetainedLayerPlan {
        cache_key,
        scene,
        snapshot: retained,
        diff_plan,
        reused,
        content_replay_damage,
    })
}

/// Plan an extension's before-content layer, when it has a scene there.
pub(super) fn build_before_content_extension_output_plan(
    snapshot: &ExtensionLayerSnapshot,
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> Option<BeforeContentExtensionOutputPlan> {
    let plan = retained_layer_plan(snapshot, content, policy, render_context, pane_buffers)?;
    let output_damage = merge_render_damage(
        merge_render_damage(
            plan.diff_plan.update_damage.clone(),
            plan.diff_plan.stale_cleanup_damage.clone(),
            snapshot.surface_rect,
            policy,
        ),
        plan.content_replay_damage.clone(),
        snapshot.surface_rect,
        policy,
    );
    let output_items = retained_scene_items_to_render_items(
        retained_scene_items_for_render_damage(&plan.snapshot.items, &output_damage),
    );
    Some(BeforeContentExtensionOutputPlan {
        snapshot: snapshot.clone(),
        cache_key: plan.cache_key,
        damage_rects: before_content_damage_rects(&output_damage, content),
        output: RetainedLayerOutput {
            diff_plan: Box::new(plan.diff_plan),
            snapshot: plan.snapshot,
            scene: plan.scene,
            reused: plan.reused,
            content_replay_damage: plan.content_replay_damage,
            output_damage,
            output_items,
        },
    })
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
    /// Where the layers' items were and no longer are: cleared before pane content repaints.
    pub(super) retained_cleanup_damage: RenderDamage,
}

#[derive(Clone)]
pub(super) struct AfterContentExtensionOutputPlan {
    pub(super) surface_index: usize,
    pub(super) snapshot: ExtensionLayerSnapshot,
    pub(super) cache_key: (String, Uuid),
    pub(super) output: RetainedLayerOutput,
}

pub(super) fn build_after_content_surface_output_plan(
    surface_index: usize,
    layer_snapshots: &[ExtensionLayerSnapshot],
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> AfterContentSurfaceOutputPlan {
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
            match &plan.output.diff_plan.stale_cleanup_damage {
                RenderDamage::None => {}
                RenderDamage::FullSurface => {
                    retained_cleanup_regions.clear();
                    retained_cleanup_regions.push(snapshot.surface_rect);
                }
                RenderDamage::Regions(regions) => {
                    retained_cleanup_regions.extend(regions.iter().copied());
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
        retained_cleanup_damage,
    }
}

/// Plan an extension's after-content layer, when it has a scene there.
pub(super) fn build_after_content_extension_output_plan(
    surface_index: usize,
    snapshot: &ExtensionLayerSnapshot,
    content: PaneRect,
    policy: DamageCoalescingPolicy,
    render_context: &RenderExtensionContext,
    pane_buffers: &BTreeMap<Uuid, PaneRenderBuffer>,
) -> Option<AfterContentExtensionOutputPlan> {
    let plan = retained_layer_plan(snapshot, content, policy, render_context, pane_buffers)?;
    let output_damage = merge_render_damage(
        plan.diff_plan.update_damage.clone(),
        plan.content_replay_damage.clone(),
        snapshot.surface_rect,
        policy,
    );
    let retained_output_damage = merge_render_damage(
        output_damage.clone(),
        plan.diff_plan.stale_cleanup_damage.clone(),
        snapshot.surface_rect,
        policy,
    );
    let output_items = retained_scene_items_to_render_items(
        retained_scene_items_for_render_damage(&plan.snapshot.items, &retained_output_damage),
    );
    Some(AfterContentExtensionOutputPlan {
        surface_index,
        snapshot: snapshot.clone(),
        cache_key: plan.cache_key,
        output: RetainedLayerOutput {
            diff_plan: Box::new(plan.diff_plan),
            snapshot: plan.snapshot,
            scene: plan.scene,
            reused: plan.reused,
            content_replay_damage: plan.content_replay_damage,
            output_damage,
            output_items,
        },
    })
}
