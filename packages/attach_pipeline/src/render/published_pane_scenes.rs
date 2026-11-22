//! Pane-attached published surfaces, painted through the retained extension
//! engine.
//!
//! A surface published with `PluginSurfaceTarget::Pane { surface, depth }` is
//! drawn in that pane's own stages: under its content (showing only through
//! transparent pane cells and keeping the pane's background) or over it. This
//! adapter presents every such surface to the retained extension pipeline, which
//! owns keyed diffing, stale cleanup, under-cell blending and terminal graphics,
//! so published producers get exactly the behaviour render extensions have,
//! without a per-frame callback.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};

use bmux_plugin::surface::{PluginSurface, PluginSurfaceRegistry, PluginSurfaceTarget};
use bmux_plugin::{
    AttachRenderExtension, ExtensionRect, RenderExtensionContext, RenderExtensionLayer,
    RenderLayerItem, RenderLayerScene, RenderOp, RenderSceneItem, RenderSceneItemKey,
    RenderSurfaceRole,
};
use bmux_presentation_scene::scene::{CellBlock, PaneDepth, SceneCell, SceneSymbol};
use uuid::Uuid;

use super::visible_segments::render_ops_to_cells;

/// What one pane shows at one depth: the published surfaces in stacking order.
#[derive(Default)]
struct PaneLayer {
    revision: u64,
    surfaces: Vec<Arc<PluginSurface>>,
}

/// Adapter from a surface registry to the retained extension pipeline. Hosts
/// keep one per registry for the life of the view: the pipeline reuses what an
/// extension sampled by its identity.
pub struct PublishedPaneScenes {
    registry: Arc<PluginSurfaceRegistry>,
    layers: RwLock<BTreeMap<(Uuid, bool), PaneLayer>>,
}

impl PublishedPaneScenes {
    #[must_use]
    pub const fn new(registry: Arc<PluginSurfaceRegistry>) -> Self {
        Self {
            registry,
            layers: RwLock::new(BTreeMap::new()),
        }
    }

    /// Whether this adapter reads `registry`.
    #[must_use]
    pub fn reads(&self, registry: &Arc<PluginSurfaceRegistry>) -> bool {
        Arc::ptr_eq(&self.registry, registry)
    }

    fn layer_key(surface: Uuid, layer: RenderExtensionLayer) -> (Uuid, bool) {
        (surface, layer == RenderExtensionLayer::BeforePaneContent)
    }
}

const fn depth_is_under(depth: PaneDepth) -> bool {
    matches!(depth, PaneDepth::UnderContent)
}

impl AttachRenderExtension for PublishedPaneScenes {
    fn name(&self) -> &'static str {
        "bmux.presentation.pane-scenes"
    }

    fn refresh_state(&self) {
        let mut layers: BTreeMap<(Uuid, bool), PaneLayer> = BTreeMap::new();
        for surface in self.registry.surfaces() {
            let PluginSurfaceTarget::Pane {
                surface: pane,
                depth,
            } = surface.target
            else {
                continue;
            };
            if !surface.visible {
                continue;
            }
            layers
                .entry((pane, depth_is_under(depth)))
                .or_default()
                .surfaces
                .push(Arc::new(surface));
        }
        for layer in layers.values_mut() {
            layer.surfaces.sort_by(|left, right| {
                (
                    left.layer,
                    left.z,
                    &left.id.owner_plugin_id,
                    &left.id.local_id,
                )
                    .cmp(&(
                        right.layer,
                        right.z,
                        &right.id.owner_plugin_id,
                        &right.id.local_id,
                    ))
            });
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for surface in &layer.surfaces {
                surface.id.owner_plugin_id.hash(&mut hasher);
                surface.id.local_id.hash(&mut hasher);
                surface.revision.hash(&mut hasher);
            }
            // Zero is reserved for "nothing published here".
            layer.revision = hasher.finish().max(1);
        }
        if let Ok(mut current) = self.layers.write() {
            *current = layers;
        }
    }

    fn render_layer_revision(&self, surface_id: Uuid, layer: RenderExtensionLayer) -> Option<u64> {
        let layers = self.layers.read().ok()?;
        Some(
            layers
                .get(&Self::layer_key(surface_id, layer))
                .map_or(0, |layer| layer.revision),
        )
    }

    fn render_layer_scene_with_context(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<RenderLayerScene> {
        if context.surface_role != RenderSurfaceRole::Content {
            return None;
        }
        // Copy out the layer's surfaces (cheap `Arc`s) so the lock is released
        // before the scene is built.
        let (revision, surfaces) = self
            .layers
            .read()
            .ok()?
            .get(&Self::layer_key(surface_id, layer))
            .map(|published| (published.revision, published.surfaces.clone()))?;
        let occluders = if layer == RenderExtensionLayer::AfterPaneContent {
            context.opaque_occluders.as_slice()
        } else {
            // Under-content cells are only shown through the pane's own cells.
            &[]
        };
        let mut builder = RenderLayerScene::builder().revision(revision);
        for surface in &surfaces {
            let prefix = format!("{}/{}", surface.id.owner_plugin_id, surface.id.local_id);
            let z = i16::try_from(surface.z.clamp(i32::from(i16::MIN), i32::from(i16::MAX)))
                .unwrap_or_default();
            for (index, op) in surface.ops.iter().enumerate() {
                let op = translate_op(op, surface_rect.x, surface_rect.y);
                if let Some(item) = clip_item(
                    RenderSceneItem::op(
                        RenderSceneItemKey::new(format!("{prefix}/op/{index}")),
                        z,
                        op,
                    ),
                    occluders,
                ) {
                    builder = builder.item(item);
                }
            }
            for item in &surface.items {
                let translated = RenderSceneItem {
                    key: RenderSceneItemKey::new(format!("{prefix}/{}", item.key.0)),
                    z: item.z,
                    item: match &item.item {
                        RenderLayerItem::Op(op) => {
                            RenderLayerItem::Op(translate_op(op, surface_rect.x, surface_rect.y))
                        }
                        RenderLayerItem::Graphic(graphic) => {
                            let mut graphic = graphic.clone();
                            graphic.cell_rect =
                                translate_rect(graphic.cell_rect, surface_rect.x, surface_rect.y);
                            RenderLayerItem::Graphic(graphic)
                        }
                    },
                };
                if let Some(item) = clip_item(translated, occluders) {
                    builder = builder.item(item);
                }
            }
        }
        Some(builder.build())
    }
}

const fn translate_rect(rect: ExtensionRect, x: u16, y: u16) -> ExtensionRect {
    ExtensionRect::new(
        rect.x.saturating_add(x),
        rect.y.saturating_add(y),
        rect.w,
        rect.h,
    )
}

fn translate_op(op: &RenderOp, x: u16, y: u16) -> RenderOp {
    match op {
        RenderOp::TextRun {
            x: ox,
            y: oy,
            text,
            style,
        } => RenderOp::TextRun {
            x: ox.saturating_add(x),
            y: oy.saturating_add(y),
            text: text.clone(),
            style: *style,
        },
        RenderOp::StyledText {
            x: ox,
            y: oy,
            spans,
        } => RenderOp::StyledText {
            x: ox.saturating_add(x),
            y: oy.saturating_add(y),
            spans: spans.clone(),
        },
        RenderOp::ClearRect { rect, style } => RenderOp::ClearRect {
            rect: translate_rect(*rect, x, y),
            style: *style,
        },
        RenderOp::EraseRowSegment {
            x: ox,
            y: oy,
            width,
            style,
        } => RenderOp::EraseRowSegment {
            x: ox.saturating_add(x),
            y: oy.saturating_add(y),
            width: *width,
            style: *style,
        },
        RenderOp::FillRect { rect, ch, style } => RenderOp::FillRect {
            rect: translate_rect(*rect, x, y),
            ch: *ch,
            style: *style,
        },
        RenderOp::Border {
            rect,
            glyphs,
            style,
        } => RenderOp::Border {
            rect: translate_rect(*rect, x, y),
            glyphs: *glyphs,
            style: *style,
        },
        RenderOp::CellGrid { x: ox, y: oy, rows } => RenderOp::CellGrid {
            x: ox.saturating_add(x),
            y: oy.saturating_add(y),
            rows: rows.clone(),
        },
        RenderOp::Cells(block) => {
            let mut block = block.clone();
            block.x = block.x.saturating_add(x);
            block.y = block.y.saturating_add(y);
            RenderOp::Cells(block)
        }
    }
}

/// Keep an item off the cells overlays cover: the pipeline repaints pane items
/// independently of the overlays above them. An op that touches an occluder is
/// rasterized into a cell block whose covered cells are transparent; a graphic
/// that touches one is dropped (it cannot be partially shown).
fn clip_item(item: RenderSceneItem, occluders: &[ExtensionRect]) -> Option<RenderSceneItem> {
    let bounds = item.item.painted_rects();
    let touched = occluders
        .iter()
        .any(|occluder| bounds.iter().any(|rect| rect.intersects(*occluder)));
    if !touched {
        return Some(item);
    }
    let RenderLayerItem::Op(op) = &item.item else {
        return None;
    };
    let block = match op {
        RenderOp::Cells(block) => clip_block(block.clone(), occluders),
        op => {
            let area = op_bounds(op)?;
            let mut block = CellBlock::transparent(area.x, area.y, area.w, area.h);
            for ((x, y), cell) in render_ops_to_cells(area, std::slice::from_ref(op)) {
                block.set(
                    x - area.x,
                    y - area.y,
                    SceneCell {
                        symbol: Some(SceneSymbol::Char(cell.ch)),
                        style: cell.style,
                    },
                );
            }
            clip_block(block, occluders)
        }
    };
    Some(RenderSceneItem::op(
        item.key,
        item.z,
        RenderOp::Cells(block),
    ))
}

fn op_bounds(op: &RenderOp) -> Option<ExtensionRect> {
    let rects = op.painted_rects();
    let first = *rects.first()?;
    Some(rects.iter().skip(1).fold(first, |union, rect| {
        let x = union.x.min(rect.x);
        let y = union.y.min(rect.y);
        let right = union.right().max(rect.right());
        let bottom = union.bottom().max(rect.bottom());
        ExtensionRect::new(x, y, right - x, bottom - y)
    }))
}

fn clip_block(mut block: CellBlock, occluders: &[ExtensionRect]) -> CellBlock {
    for row in 0..block.height {
        for column in 0..block.width {
            let Some(width) = block
                .get(column, row)
                .and_then(|cell| cell.symbol.as_ref())
                .map(|symbol| symbol.width().max(1))
            else {
                continue;
            };
            let covered = ExtensionRect::new(
                block.x.saturating_add(column),
                block.y.saturating_add(row),
                width,
                1,
            );
            if occluders
                .iter()
                .any(|occluder| occluder.intersects(covered))
            {
                let style = block
                    .get(column, row)
                    .map(|cell| cell.style)
                    .unwrap_or_default();
                block.set(
                    column,
                    row,
                    SceneCell {
                        symbol: None,
                        style,
                    },
                );
            }
        }
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::RenderStyle;
    use bmux_plugin::surface::{PluginSurfaceId, PluginSurfaceSnapshot};

    const OWNER: &str = "test.pane-scenes";

    fn publish(registry: &PluginSurfaceRegistry, revision: u64, pane: Uuid, text: &str) {
        registry
            .publish(
                OWNER,
                PluginSurfaceSnapshot {
                    revision,
                    surfaces: vec![PluginSurface::pane(
                        PluginSurfaceId::new(OWNER, "badge", Uuid::from_u128(1)),
                        revision,
                        pane,
                        PaneDepth::OverContent,
                        vec![RenderSceneItem::op(
                            RenderSceneItemKey::new("text"),
                            0,
                            RenderOp::text_run(1, 1, text, RenderStyle::new()),
                        )],
                    )],
                },
            )
            .unwrap();
    }

    fn context(occluders: Vec<ExtensionRect>) -> RenderExtensionContext {
        RenderExtensionContext {
            surface_role: RenderSurfaceRole::Content,
            opaque_occluders: occluders,
            ..RenderExtensionContext::default()
        }
    }

    #[test]
    fn published_items_are_translated_revisioned_and_clipped_to_occluders() {
        let registry = Arc::new(PluginSurfaceRegistry::new(8));
        let scenes = PublishedPaneScenes::new(registry.clone());
        let pane = Uuid::from_u128(7);
        let other = Uuid::from_u128(8);
        let after = RenderExtensionLayer::AfterPaneContent;
        publish(&registry, 1, pane, "abcd");
        scenes.refresh_state();
        let first = scenes.render_layer_revision(pane, after).unwrap();
        assert_ne!(first, 0);
        assert_eq!(scenes.render_layer_revision(other, after), Some(0));
        assert_eq!(
            scenes.render_layer_revision(pane, RenderExtensionLayer::BeforePaneContent),
            Some(0)
        );
        let rect = ExtensionRect::new(10, 5, 20, 5);
        let scene = scenes
            .render_layer_scene_with_context(pane, &rect, after, &context(Vec::new()))
            .unwrap();
        assert_eq!(
            scene.items[0].item,
            RenderLayerItem::Op(RenderOp::text_run(11, 6, "abcd", RenderStyle::new()))
        );
        // Overlay surfaces are not painted by pane layers.
        let overlay = RenderExtensionContext {
            surface_role: RenderSurfaceRole::Overlay,
            ..context(Vec::new())
        };
        assert!(
            scenes
                .render_layer_scene_with_context(pane, &rect, after, &overlay)
                .is_none()
        );
        // An occluder over the middle two cells leaves them transparent.
        let scene = scenes
            .render_layer_scene_with_context(
                pane,
                &rect,
                after,
                &context(vec![ExtensionRect::new(12, 6, 2, 1)]),
            )
            .unwrap();
        let RenderLayerItem::Op(RenderOp::Cells(block)) = &scene.items[0].item else {
            panic!("an occluded op is clipped into cells: {:?}", scene.items[0]);
        };
        let shown = (0..block.width)
            .map(|column| block.get(column, 0).unwrap().symbol.is_some())
            .collect::<Vec<_>>();
        assert_eq!(shown, [true, false, false, true]);
        // A new publication changes the revision the pipeline sees.
        publish(&registry, 2, pane, "abce");
        scenes.refresh_state();
        assert_ne!(scenes.render_layer_revision(pane, after), Some(first));
    }
}
