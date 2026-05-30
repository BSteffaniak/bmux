use crate::types::{
    ExtensionRetainedItemCacheEntry, ExtensionRetainedLayerCacheEntry, PaneRect, PaneRenderBuffer,
};
use bmux_plugin::{
    BorderGlyphs, ExtensionRect, RenderCell, RenderDamage, RenderLayerItem,
    RenderLayerScene as PluginRenderLayerScene, RenderOp, RenderSceneItem as PluginRenderSceneItem,
    RenderSceneItemKind as PluginRenderSceneItemKind, RenderStyle, RenderTextSpan, RenderUnderCell,
    TerminalGraphicOverlay, TerminalRenderCapabilities, render_text_width_u16,
};
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use super::{DamageCoalescingPolicy, DamageRect, coalesce_render_damage};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct RenderSceneItemKey(String);

impl RenderSceneItemKey {
    pub(super) fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RenderLayerScene {
    pub(super) revision: Option<u64>,
    pub(super) items: Vec<RenderSceneItem>,
}

impl RenderLayerScene {
    pub(super) const fn new(revision: Option<u64>, items: Vec<RenderSceneItem>) -> Self {
        Self { revision, items }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RenderSceneItem {
    pub(super) key: RenderSceneItemKey,
    pub(super) z: i16,
    pub(super) bounds: ExtensionRect,
    pub(super) content_fingerprint: u64,
    pub(super) kind: RenderSceneItemKind,
}

impl RenderSceneItem {
    pub(super) fn text(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        text: impl Into<String>,
        style: RenderStyle,
    ) -> Self {
        let text = text.into();
        let bounds = ExtensionRect::new(
            x,
            y,
            render_text_width_u16(&text),
            u16::from(!text.is_empty()),
        );
        Self::new(
            key,
            z,
            bounds,
            RenderSceneItemKind::Text { x, y, text, style },
        )
    }

    pub(super) fn styled_text(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        spans: Vec<RenderTextSpan>,
    ) -> Self {
        let width = spans
            .iter()
            .map(|span| render_text_width_u16(&span.text))
            .fold(0_u16, u16::saturating_add);
        let bounds = ExtensionRect::new(x, y, width, u16::from(width > 0));
        Self::new(
            key,
            z,
            bounds,
            RenderSceneItemKind::StyledText { x, y, spans },
        )
    }

    pub(super) fn fill_rect(
        key: impl Into<String>,
        z: i16,
        rect: ExtensionRect,
        ch: char,
        style: RenderStyle,
    ) -> Self {
        Self::new(
            key,
            z,
            rect,
            RenderSceneItemKind::FillRect { rect, ch, style },
        )
    }

    pub(super) fn border(
        key: impl Into<String>,
        z: i16,
        rect: ExtensionRect,
        glyphs: BorderGlyphs,
        style: RenderStyle,
    ) -> Self {
        Self::new(
            key,
            z,
            rect,
            RenderSceneItemKind::Border {
                rect,
                glyphs,
                style,
            },
        )
    }

    pub(super) fn cell_grid(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        rows: Vec<Vec<RenderCell>>,
    ) -> Self {
        let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
        let width = rows
            .iter()
            .map(|row| u16::try_from(row.len()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);
        let bounds = ExtensionRect::new(x, y, width, height);
        Self::new(key, z, bounds, RenderSceneItemKind::CellGrid { x, y, rows })
    }

    pub(super) fn terminal_graphic(
        key: impl Into<String>,
        z: i16,
        graphic: TerminalGraphicOverlay,
    ) -> Self {
        let bounds = graphic.cell_rect;
        Self::new(
            key,
            z,
            bounds,
            RenderSceneItemKind::TerminalGraphic { graphic },
        )
    }

    pub(super) fn under_cells(
        key: impl Into<String>,
        z: i16,
        cells: Vec<(u16, u16, RenderUnderCell)>,
    ) -> Self {
        let bounds = under_cell_bounds(&cells);
        Self::new(key, z, bounds, RenderSceneItemKind::UnderCells { cells })
    }

    fn new(
        key: impl Into<String>,
        z: i16,
        bounds: ExtensionRect,
        kind: RenderSceneItemKind,
    ) -> Self {
        let key = RenderSceneItemKey::new(key);
        let content_fingerprint = retained_item_content_fingerprint(z, bounds, &kind);
        Self {
            key,
            z,
            bounds,
            content_fingerprint,
            kind,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RenderSceneItemKind {
    Text {
        x: u16,
        y: u16,
        text: String,
        style: RenderStyle,
    },
    StyledText {
        x: u16,
        y: u16,
        spans: Vec<RenderTextSpan>,
    },
    FillRect {
        rect: ExtensionRect,
        ch: char,
        style: RenderStyle,
    },
    Border {
        rect: ExtensionRect,
        glyphs: BorderGlyphs,
        style: RenderStyle,
    },
    CellGrid {
        x: u16,
        y: u16,
        rows: Vec<Vec<RenderCell>>,
    },
    TerminalGraphic {
        graphic: TerminalGraphicOverlay,
    },
    UnderCells {
        cells: Vec<(u16, u16, RenderUnderCell)>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExtensionRetainedLayerSnapshot {
    pub(super) surface_rect: ExtensionRect,
    pub(super) revision: Option<u64>,
    pub(super) items: Vec<RenderSceneItem>,
}

impl ExtensionRetainedLayerSnapshot {
    pub(super) const fn new(
        surface_rect: ExtensionRect,
        revision: Option<u64>,
        items: Vec<RenderSceneItem>,
    ) -> Self {
        Self {
            surface_rect,
            revision,
            items,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExtensionLayerDiffPlan {
    pub(super) update_damage: RenderDamage,
    pub(super) stale_cleanup_damage: RenderDamage,
    pub(super) content_replay_damage: Vec<DamageRect>,
    pub(super) unchanged_items: Vec<RenderSceneItemKey>,
    pub(super) changed_items: Vec<RenderSceneItemKey>,
    pub(super) added_items: Vec<RenderSceneItemKey>,
    pub(super) removed_items: Vec<RenderSceneItemKey>,
    pub(super) output_items: Vec<RenderSceneItem>,
}

pub(super) fn retained_snapshot_from_plugin_scene(
    surface_rect: ExtensionRect,
    scene: &PluginRenderLayerScene,
) -> ExtensionRetainedLayerSnapshot {
    ExtensionRetainedLayerSnapshot::new(
        surface_rect,
        scene.revision,
        scene
            .items
            .iter()
            .map(retained_item_from_plugin_item)
            .collect(),
    )
}

pub(super) fn retained_scene_items_to_render_items(
    items: &[RenderSceneItem],
) -> Vec<RenderLayerItem> {
    items
        .iter()
        .filter_map(retained_scene_item_to_render_item)
        .collect()
}

fn retained_item_from_plugin_item(item: &PluginRenderSceneItem) -> RenderSceneItem {
    let bounds = plugin_scene_item_bounds(item);
    let kind = retained_item_kind_from_plugin_kind(&item.kind);
    RenderSceneItem::new(item.key.0.clone(), item.z, bounds, kind)
}

fn plugin_scene_item_bounds(item: &PluginRenderSceneItem) -> ExtensionRect {
    match &item.kind {
        PluginRenderSceneItemKind::Text { x, y, text, .. } => ExtensionRect::new(
            *x,
            *y,
            render_text_width_u16(text),
            u16::from(!text.is_empty()),
        ),
        PluginRenderSceneItemKind::StyledText { x, y, spans } => {
            let width = spans
                .iter()
                .map(|span| render_text_width_u16(&span.text))
                .fold(0_u16, u16::saturating_add);
            ExtensionRect::new(*x, *y, width, u16::from(width > 0))
        }
        PluginRenderSceneItemKind::FillRect { rect, .. }
        | PluginRenderSceneItemKind::Border { rect, .. } => *rect,
        PluginRenderSceneItemKind::CellGrid { x, y, rows } => {
            let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
            let width = rows
                .iter()
                .map(|row| u16::try_from(row.len()).unwrap_or(u16::MAX))
                .max()
                .unwrap_or(0);
            ExtensionRect::new(*x, *y, width, height)
        }
        PluginRenderSceneItemKind::TerminalGraphic { graphic } => graphic.cell_rect,
        PluginRenderSceneItemKind::UnderCells { cells } => under_cell_bounds(cells),
    }
}

fn retained_item_kind_from_plugin_kind(kind: &PluginRenderSceneItemKind) -> RenderSceneItemKind {
    match kind {
        PluginRenderSceneItemKind::Text { x, y, text, style } => RenderSceneItemKind::Text {
            x: *x,
            y: *y,
            text: text.clone(),
            style: *style,
        },
        PluginRenderSceneItemKind::StyledText { x, y, spans } => RenderSceneItemKind::StyledText {
            x: *x,
            y: *y,
            spans: spans.clone(),
        },
        PluginRenderSceneItemKind::FillRect { rect, ch, style } => RenderSceneItemKind::FillRect {
            rect: *rect,
            ch: *ch,
            style: *style,
        },
        PluginRenderSceneItemKind::Border {
            rect,
            glyphs,
            style,
        } => RenderSceneItemKind::Border {
            rect: *rect,
            glyphs: *glyphs,
            style: *style,
        },
        PluginRenderSceneItemKind::CellGrid { x, y, rows } => RenderSceneItemKind::CellGrid {
            x: *x,
            y: *y,
            rows: rows.clone(),
        },
        PluginRenderSceneItemKind::TerminalGraphic { graphic } => {
            RenderSceneItemKind::TerminalGraphic {
                graphic: graphic.clone(),
            }
        }
        PluginRenderSceneItemKind::UnderCells { cells } => RenderSceneItemKind::UnderCells {
            cells: cells.clone(),
        },
    }
}

fn retained_scene_item_to_render_item(item: &RenderSceneItem) -> Option<RenderLayerItem> {
    match &item.kind {
        RenderSceneItemKind::Text { x, y, text, style } => {
            Some(RenderLayerItem::Op(RenderOp::TextRun {
                x: *x,
                y: *y,
                text: text.clone(),
                style: *style,
            }))
        }
        RenderSceneItemKind::StyledText { x, y, spans } => {
            Some(RenderLayerItem::Op(RenderOp::StyledText {
                x: *x,
                y: *y,
                spans: spans.clone(),
            }))
        }
        RenderSceneItemKind::FillRect { rect, ch, style } => {
            Some(RenderLayerItem::Op(RenderOp::FillRect {
                rect: *rect,
                ch: *ch,
                style: *style,
            }))
        }
        RenderSceneItemKind::Border {
            rect,
            glyphs,
            style,
        } => Some(RenderLayerItem::Op(RenderOp::Border {
            rect: *rect,
            glyphs: *glyphs,
            style: *style,
        })),
        RenderSceneItemKind::CellGrid { x, y, rows } => {
            Some(RenderLayerItem::Op(RenderOp::CellGrid {
                x: *x,
                y: *y,
                rows: rows.clone(),
            }))
        }
        RenderSceneItemKind::TerminalGraphic { graphic } => {
            Some(RenderLayerItem::Graphic(graphic.clone()))
        }
        RenderSceneItemKind::UnderCells { .. } => None,
    }
}

pub(super) fn retained_layer_cache_key(
    extension_name: &str,
    surface_id: uuid::Uuid,
    layer: bmux_plugin::RenderExtensionLayer,
    capabilities: TerminalRenderCapabilities,
) -> (String, uuid::Uuid) {
    (
        format!(
            "{}::{:?}::retained::caps={}",
            extension_name,
            layer,
            capabilities.cache_key()
        ),
        surface_id,
    )
}

pub(super) fn load_retained_layer_snapshot(
    pane_buffer: Option<&PaneRenderBuffer>,
    key: &(String, uuid::Uuid),
) -> Option<ExtensionRetainedLayerSnapshot> {
    let entry = pane_buffer?.extension_retained_layer_cache.get(key)?;
    Some(ExtensionRetainedLayerSnapshot::new(
        entry.surface_rect,
        entry.revision,
        entry
            .items
            .iter()
            .map(retained_item_from_cache_entry)
            .collect(),
    ))
}

pub(super) fn commit_retained_layer_snapshot(
    pane_buffers: &mut BTreeMap<uuid::Uuid, PaneRenderBuffer>,
    pane_id: uuid::Uuid,
    key: (String, uuid::Uuid),
    surface_id: uuid::Uuid,
    layer: bmux_plugin::RenderExtensionLayer,
    snapshot: &ExtensionRetainedLayerSnapshot,
) {
    let Some(buffer) = pane_buffers.get_mut(&pane_id) else {
        return;
    };
    buffer.extension_retained_layer_cache.insert(
        key,
        ExtensionRetainedLayerCacheEntry {
            surface_id,
            surface_rect: snapshot.surface_rect,
            layer,
            revision: snapshot.revision,
            items: snapshot
                .items
                .iter()
                .map(retained_item_cache_entry)
                .collect(),
        },
    );
}

pub(super) fn evict_retained_layer_snapshots_for_surface(
    pane_buffers: &mut BTreeMap<uuid::Uuid, PaneRenderBuffer>,
    pane_id: uuid::Uuid,
    surface_id: uuid::Uuid,
) {
    let Some(buffer) = pane_buffers.get_mut(&pane_id) else {
        return;
    };
    buffer
        .extension_retained_layer_cache
        .retain(|_, entry| entry.surface_id != surface_id);
}

pub(super) fn build_extension_layer_diff_plan(
    previous: Option<&ExtensionRetainedLayerSnapshot>,
    current: Option<&ExtensionRetainedLayerSnapshot>,
    content_rect: PaneRect,
    policy: DamageCoalescingPolicy,
) -> ExtensionLayerDiffPlan {
    let mut update_rects = Vec::new();
    let mut cleanup_rects = Vec::new();
    let mut unchanged_items = Vec::new();
    let mut changed_items = Vec::new();
    let mut added_items = Vec::new();
    let mut removed_items = Vec::new();

    match (previous, current) {
        (None, None) => {}
        (None, Some(current)) => {
            update_rects.extend(current.items.iter().map(|item| item.bounds));
            added_items.extend(current.items.iter().map(|item| item.key.clone()));
        }
        (Some(previous), None) => {
            cleanup_rects.extend(previous.items.iter().map(|item| item.bounds));
            removed_items.extend(previous.items.iter().map(|item| item.key.clone()));
        }
        (Some(previous), Some(current)) if previous.surface_rect != current.surface_rect => {
            cleanup_rects.push(previous.surface_rect);
            update_rects.extend(current.items.iter().map(|item| item.bounds));
            changed_items.extend(current.items.iter().map(|item| item.key.clone()));
            removed_items.extend(previous.items.iter().map(|item| item.key.clone()));
        }
        (Some(previous), Some(current)) => {
            let previous_items = previous
                .items
                .iter()
                .map(|item| (&item.key, item))
                .collect::<BTreeMap<_, _>>();
            let current_items = current
                .items
                .iter()
                .map(|item| (&item.key, item))
                .collect::<BTreeMap<_, _>>();

            for item in &current.items {
                match previous_items.get(&item.key) {
                    None => {
                        update_rects.push(item.bounds);
                        added_items.push(item.key.clone());
                    }
                    Some(previous_item) if retained_items_equivalent(previous_item, item) => {
                        unchanged_items.push(item.key.clone());
                    }
                    Some(previous_item) => {
                        update_rects.push(item.bounds);
                        cleanup_rects.extend(rect_difference(previous_item.bounds, item.bounds));
                        changed_items.push(item.key.clone());
                    }
                }
            }

            for item in &previous.items {
                if !current_items.contains_key(&item.key) {
                    cleanup_rects.push(item.bounds);
                    removed_items.push(item.key.clone());
                }
            }
        }
    }

    let update_surface_rect = current
        .map_or_else(
            || previous.map(|snapshot| snapshot.surface_rect),
            |snapshot| Some(snapshot.surface_rect),
        )
        .unwrap_or_else(|| ExtensionRect::new(0, 0, 0, 0));
    let cleanup_surface_rect = previous
        .map_or_else(
            || current.map(|snapshot| snapshot.surface_rect),
            |snapshot| Some(snapshot.surface_rect),
        )
        .unwrap_or_else(|| ExtensionRect::new(0, 0, 0, 0));
    let update_damage = coalesce_render_damage(
        RenderDamage::from_rects(update_rects),
        update_surface_rect,
        policy,
    );
    let stale_cleanup_damage = coalesce_render_damage(
        RenderDamage::from_rects(cleanup_rects),
        cleanup_surface_rect,
        policy,
    );
    let content_replay_damage = render_damage_content_rects(&stale_cleanup_damage, content_rect);
    ExtensionLayerDiffPlan {
        update_damage,
        stale_cleanup_damage,
        content_replay_damage,
        unchanged_items,
        changed_items,
        added_items,
        removed_items,
        output_items: current.map_or_else(Vec::new, |snapshot| snapshot.items.clone()),
    }
}

fn retained_item_cache_entry(item: &RenderSceneItem) -> ExtensionRetainedItemCacheEntry {
    ExtensionRetainedItemCacheEntry {
        key: item.key.0.clone(),
        z: item.z,
        bounds: item.bounds,
        content_fingerprint: item.content_fingerprint,
    }
}

fn retained_item_from_cache_entry(entry: &ExtensionRetainedItemCacheEntry) -> RenderSceneItem {
    RenderSceneItem {
        key: RenderSceneItemKey::new(entry.key.clone()),
        z: entry.z,
        bounds: entry.bounds,
        content_fingerprint: entry.content_fingerprint,
        kind: RenderSceneItemKind::UnderCells { cells: Vec::new() },
    }
}

fn retained_items_equivalent(previous: &RenderSceneItem, current: &RenderSceneItem) -> bool {
    previous.z == current.z
        && previous.bounds == current.bounds
        && previous.content_fingerprint == current.content_fingerprint
}

fn rect_difference(old: ExtensionRect, new: ExtensionRect) -> Vec<ExtensionRect> {
    if old.is_empty() || rect_contains_rect(new, old) {
        return Vec::new();
    }
    if new.is_empty() || !old.intersects(new) {
        return vec![old];
    }
    let x1 = old.x.max(new.x);
    let y1 = old.y.max(new.y);
    let x2 = old.right().min(new.right());
    let y2 = old.bottom().min(new.bottom());
    let intersection = ExtensionRect::new(x1, y1, x2.saturating_sub(x1), y2.saturating_sub(y1));
    let candidates = [
        ExtensionRect::new(old.x, old.y, old.w, intersection.y.saturating_sub(old.y)),
        ExtensionRect::new(
            old.x,
            intersection.bottom(),
            old.w,
            old.bottom().saturating_sub(intersection.bottom()),
        ),
        ExtensionRect::new(
            old.x,
            intersection.y,
            intersection.x.saturating_sub(old.x),
            intersection.h,
        ),
        ExtensionRect::new(
            intersection.right(),
            intersection.y,
            old.right().saturating_sub(intersection.right()),
            intersection.h,
        ),
    ];
    candidates
        .into_iter()
        .filter(|rect| !rect.is_empty())
        .collect()
}

const fn rect_contains_rect(outer: ExtensionRect, inner: ExtensionRect) -> bool {
    inner.is_empty()
        || (!outer.is_empty()
            && inner.x >= outer.x
            && inner.y >= outer.y
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom())
}

fn render_damage_content_rects(damage: &RenderDamage, content: PaneRect) -> Vec<DamageRect> {
    let rects = match damage {
        RenderDamage::None => Vec::new(),
        RenderDamage::FullSurface => vec![ExtensionRect::new(
            content.x, content.y, content.w, content.h,
        )],
        RenderDamage::Regions(regions) => regions.clone(),
    };
    rects
        .into_iter()
        .filter_map(|rect| {
            let x1 = rect.x.max(content.x);
            let y1 = rect.y.max(content.y);
            let x2 = rect.right().min(content.x.saturating_add(content.w));
            let y2 = rect.bottom().min(content.y.saturating_add(content.h));
            (x1 < x2 && y1 < y2).then_some(DamageRect::new(
                x1.saturating_sub(content.x),
                y1.saturating_sub(content.y),
                x2.saturating_sub(x1),
                y2.saturating_sub(y1),
            ))
        })
        .collect()
}

fn under_cell_bounds(cells: &[(u16, u16, RenderUnderCell)]) -> ExtensionRect {
    let Some((first_x, first_y, _)) = cells.first() else {
        return ExtensionRect::new(0, 0, 0, 0);
    };
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (*first_x, *first_y, *first_x, *first_y);
    for (x, y, _) in cells {
        min_x = min_x.min(*x);
        min_y = min_y.min(*y);
        max_x = max_x.max(*x);
        max_y = max_y.max(*y);
    }
    ExtensionRect::new(
        min_x,
        min_y,
        max_x.saturating_sub(min_x).saturating_add(1),
        max_y.saturating_sub(min_y).saturating_add(1),
    )
}

fn retained_item_content_fingerprint(
    z: i16,
    bounds: ExtensionRect,
    kind: &RenderSceneItemKind,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    z.hash(&mut hasher);
    hash_rect(&mut hasher, bounds);
    hash_debug(&mut hasher, kind);
    hasher.finish()
}

fn hash_debug(hasher: &mut impl Hasher, value: &impl std::fmt::Debug) {
    format!("{value:?}").hash(hasher);
}

fn hash_rect(hasher: &mut impl Hasher, rect: ExtensionRect) {
    rect.x.hash(hasher);
    rect.y.hash(hasher);
    rect.w.hash(hasher);
    rect.h.hash(hasher);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_rect() -> PaneRect {
        PaneRect {
            x: 0,
            y: 0,
            w: 20,
            h: 10,
        }
    }

    fn surface_rect() -> ExtensionRect {
        ExtensionRect::new(0, 0, 20, 10)
    }

    fn text_item(key: &str, x: u16, y: u16, text: &str) -> RenderSceneItem {
        RenderSceneItem::text(key, 0, x, y, text, RenderStyle::default())
    }

    fn snapshot(items: Vec<RenderSceneItem>) -> ExtensionRetainedLayerSnapshot {
        ExtensionRetainedLayerSnapshot::new(surface_rect(), Some(1), items)
    }

    fn diff(
        previous: Option<&ExtensionRetainedLayerSnapshot>,
        current: Option<&ExtensionRetainedLayerSnapshot>,
    ) -> ExtensionLayerDiffPlan {
        build_extension_layer_diff_plan(
            previous,
            current,
            content_rect(),
            DamageCoalescingPolicy::default(),
        )
    }

    fn damage_regions(damage: &RenderDamage) -> &[ExtensionRect] {
        match damage {
            RenderDamage::Regions(regions) => regions,
            RenderDamage::None => &[],
            RenderDamage::FullSurface => panic!("expected regional damage, got full surface"),
        }
    }

    #[test]
    fn retained_layer_cache_inserts_loads_updates_and_evicts_snapshots() {
        let pane_id = uuid::Uuid::from_u128(1);
        let surface_id = uuid::Uuid::from_u128(2);
        let key = retained_layer_cache_key(
            "test.extension",
            surface_id,
            bmux_plugin::RenderExtensionLayer::AfterPaneContent,
            TerminalRenderCapabilities::default(),
        );
        let initial = snapshot(vec![text_item("header", 1, 0, "HEAD")]);
        let mut pane_buffers = BTreeMap::from([(pane_id, PaneRenderBuffer::default())]);

        commit_retained_layer_snapshot(
            &mut pane_buffers,
            pane_id,
            key.clone(),
            surface_id,
            bmux_plugin::RenderExtensionLayer::AfterPaneContent,
            &initial,
        );
        let loaded = load_retained_layer_snapshot(pane_buffers.get(&pane_id), &key)
            .expect("retained snapshot should load from cache");
        assert_eq!(loaded.surface_rect, initial.surface_rect);
        assert_eq!(loaded.revision, initial.revision);
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.items[0].key, RenderSceneItemKey::new("header"));
        assert_eq!(loaded.items[0].bounds, ExtensionRect::new(1, 0, 4, 1));

        let updated = ExtensionRetainedLayerSnapshot::new(
            surface_rect(),
            Some(2),
            vec![text_item("header", 1, 0, "HEADER")],
        );
        commit_retained_layer_snapshot(
            &mut pane_buffers,
            pane_id,
            key.clone(),
            surface_id,
            bmux_plugin::RenderExtensionLayer::AfterPaneContent,
            &updated,
        );
        let loaded = load_retained_layer_snapshot(pane_buffers.get(&pane_id), &key)
            .expect("updated retained snapshot should load from cache");
        assert_eq!(loaded.revision, Some(2));
        assert_eq!(loaded.items[0].bounds, ExtensionRect::new(1, 0, 6, 1));

        evict_retained_layer_snapshots_for_surface(&mut pane_buffers, pane_id, surface_id);
        assert!(load_retained_layer_snapshot(pane_buffers.get(&pane_id), &key).is_none());
    }

    #[test]
    fn retained_items_report_bounds_and_content_fingerprints() {
        let text = text_item("text", 2, 3, "hello");
        assert_eq!(text.bounds, ExtensionRect::new(2, 3, 5, 1));

        let changed = text_item("text", 2, 3, "HELLO");
        assert_ne!(text.content_fingerprint, changed.content_fingerprint);

        let fill = RenderSceneItem::fill_rect(
            "fill",
            0,
            ExtensionRect::new(4, 1, 3, 2),
            '#',
            RenderStyle::default(),
        );
        assert_eq!(fill.bounds, ExtensionRect::new(4, 1, 3, 2));
    }

    #[test]
    fn diff_unchanged_item_has_no_damage() {
        let previous = snapshot(vec![text_item("header", 1, 0, "HEAD")]);
        let current = snapshot(vec![text_item("header", 1, 0, "HEAD")]);

        let plan = diff(Some(&previous), Some(&current));

        assert_eq!(plan.update_damage, RenderDamage::None);
        assert_eq!(plan.stale_cleanup_damage, RenderDamage::None);
        assert_eq!(
            plan.unchanged_items,
            vec![RenderSceneItemKey::new("header")]
        );
    }

    #[test]
    fn diff_added_item_updates_new_bounds() {
        let current = snapshot(vec![text_item("header", 1, 0, "HEAD")]);

        let plan = diff(None, Some(&current));

        assert_eq!(
            damage_regions(&plan.update_damage),
            &[ExtensionRect::new(1, 0, 4, 1)]
        );
        assert_eq!(plan.stale_cleanup_damage, RenderDamage::None);
        assert_eq!(plan.added_items, vec![RenderSceneItemKey::new("header")]);
    }

    #[test]
    fn diff_removed_item_cleans_old_bounds_and_replays_content() {
        let previous = snapshot(vec![text_item("header", 1, 0, "HEAD")]);

        let plan = diff(Some(&previous), None);

        assert_eq!(plan.update_damage, RenderDamage::None);
        assert_eq!(
            damage_regions(&plan.stale_cleanup_damage),
            &[ExtensionRect::new(1, 0, 4, 1)]
        );
        assert_eq!(
            plan.content_replay_damage,
            vec![DamageRect::new(1, 0, 4, 1)]
        );
        assert_eq!(plan.removed_items, vec![RenderSceneItemKey::new("header")]);
    }

    #[test]
    fn diff_moved_item_cleans_old_and_updates_new_bounds() {
        let previous = snapshot(vec![text_item("paddle", 1, 1, "▌")]);
        let current = snapshot(vec![text_item("paddle", 1, 3, "▌")]);

        let plan = diff(Some(&previous), Some(&current));

        assert_eq!(
            damage_regions(&plan.update_damage),
            &[ExtensionRect::new(1, 3, 1, 1)]
        );
        assert_eq!(
            damage_regions(&plan.stale_cleanup_damage),
            &[ExtensionRect::new(1, 1, 1, 1)]
        );
        assert_eq!(plan.changed_items, vec![RenderSceneItemKey::new("paddle")]);
    }

    #[test]
    fn diff_changed_same_bounds_updates_without_cleanup() {
        let previous = snapshot(vec![text_item("score", 2, 0, "1")]);
        let current = snapshot(vec![text_item("score", 2, 0, "2")]);

        let plan = diff(Some(&previous), Some(&current));

        assert_eq!(
            damage_regions(&plan.update_damage),
            &[ExtensionRect::new(2, 0, 1, 1)]
        );
        assert_eq!(plan.stale_cleanup_damage, RenderDamage::None);
        assert_eq!(plan.changed_items, vec![RenderSceneItemKey::new("score")]);
    }

    #[test]
    fn diff_changed_shrink_cleans_old_and_updates_new() {
        let previous = snapshot(vec![text_item("header", 1, 0, "HEADER")]);
        let current = snapshot(vec![text_item("header", 1, 0, "HEAD")]);

        let plan = diff(Some(&previous), Some(&current));

        assert_eq!(
            damage_regions(&plan.update_damage),
            &[ExtensionRect::new(1, 0, 4, 1)]
        );
        assert_eq!(
            damage_regions(&plan.stale_cleanup_damage),
            &[ExtensionRect::new(5, 0, 2, 1)]
        );
        assert_eq!(
            plan.content_replay_damage,
            vec![DamageRect::new(5, 0, 2, 1)]
        );
    }

    #[test]
    fn diff_changed_expansion_updates_new_without_cleanup() {
        let previous = snapshot(vec![text_item("header", 1, 0, "HEAD")]);
        let current = snapshot(vec![text_item("header", 1, 0, "HEADER")]);

        let plan = diff(Some(&previous), Some(&current));

        assert_eq!(
            damage_regions(&plan.update_damage),
            &[ExtensionRect::new(1, 0, 6, 1)]
        );
        assert_eq!(plan.stale_cleanup_damage, RenderDamage::None);
    }

    #[test]
    fn diff_surface_rect_change_cleans_previous_surface_and_updates_current_items() {
        let previous = snapshot(vec![text_item("header", 1, 0, "HEAD")]);
        let current = ExtensionRetainedLayerSnapshot::new(
            ExtensionRect::new(0, 0, 30, 10),
            Some(2),
            vec![text_item("header", 1, 0, "HEAD")],
        );

        let plan = diff(Some(&previous), Some(&current));

        assert_eq!(
            damage_regions(&plan.stale_cleanup_damage),
            &[surface_rect()]
        );
        assert_eq!(
            damage_regions(&plan.update_damage),
            &[ExtensionRect::new(1, 0, 4, 1)]
        );
    }

    #[test]
    fn diff_revision_only_change_with_unchanged_items_has_no_cleanup() {
        let previous = ExtensionRetainedLayerSnapshot::new(
            surface_rect(),
            Some(1),
            vec![text_item("header", 1, 0, "HEAD")],
        );
        let current = ExtensionRetainedLayerSnapshot::new(
            surface_rect(),
            Some(2),
            vec![text_item("header", 1, 0, "HEAD")],
        );

        let plan = diff(Some(&previous), Some(&current));

        assert_eq!(plan.update_damage, RenderDamage::None);
        assert_eq!(plan.stale_cleanup_damage, RenderDamage::None);
        assert_eq!(
            plan.unchanged_items,
            vec![RenderSceneItemKey::new("header")]
        );
    }
}
