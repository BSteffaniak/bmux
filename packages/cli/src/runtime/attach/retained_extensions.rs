//! Legacy render extensions are sampled by the presentation producer, not by
//! output composition. Frame planning sees immutable per-view retained values.
use bmux_plugin::{
    AttachRenderExtension, ExtensionRect, RenderDamage, RenderExtensionContext,
    RenderExtensionLayer, RenderLayerItem, RenderLayerScene, RenderOp, RenderSurfaceChrome,
};
use std::sync::Arc;
use uuid::Uuid;

struct Layer {
    phase: RenderExtensionLayer,
    damage: RenderDamage,
    revision: Option<u64>,
    scene: Option<RenderLayerScene>,
    items: Option<Vec<RenderLayerItem>>,
    ops: Option<Vec<RenderOp>>,
}
struct Surface {
    id: Uuid,
    rect: ExtensionRect,
    chrome: RenderSurfaceChrome,
    content_override: Option<ExtensionRect>,
    under_cells: Option<Vec<(u16, u16, bmux_plugin::RenderUnderCell)>>,
    layers: Vec<Layer>,
}
struct RetainedExtension {
    name: String,
    surfaces: Vec<Surface>,
}
impl RetainedExtension {
    fn surface(&self, id: Uuid, rect: ExtensionRect) -> Option<&Surface> {
        self.surfaces
            .iter()
            .find(|surface| surface.id == id && surface.rect == rect)
    }
    fn layer(&self, id: Uuid, rect: ExtensionRect, layer: RenderExtensionLayer) -> Option<&Layer> {
        self.surface(id, rect)?
            .layers
            .iter()
            .find(|value| value.phase == layer)
    }
}
impl AttachRenderExtension for RetainedExtension {
    fn name(&self) -> &str {
        &self.name
    }
    fn surface_chrome_with_context(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        _: &RenderExtensionContext,
    ) -> RenderSurfaceChrome {
        self.surface(id, *rect)
            .map_or(RenderSurfaceChrome::Fallback, |surface| surface.chrome)
    }
    fn render_before_content_cells_with_context(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        _: &RenderDamage,
        _: &RenderExtensionContext,
    ) -> Option<Vec<(u16, u16, bmux_plugin::RenderUnderCell)>> {
        self.surface(id, *rect)
            .and_then(|surface| surface.under_cells.clone())
    }
    fn content_rect_override(&self, id: Uuid) -> Option<ExtensionRect> {
        self.surfaces
            .iter()
            .find(|surface| surface.id == id)
            .and_then(|surface| surface.content_override)
    }
    fn surface_layer_damage(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        layer: RenderExtensionLayer,
    ) -> RenderDamage {
        self.layer(id, *rect, layer)
            .map_or(RenderDamage::None, |value| value.damage.clone())
    }
    fn render_layer_revision(&self, id: Uuid, layer: RenderExtensionLayer) -> Option<u64> {
        self.surfaces
            .iter()
            .find(|surface| surface.id == id)?
            .layers
            .iter()
            .find(|value| value.phase == layer)
            .and_then(|value| value.revision)
    }
    fn render_layer_scene_with_context(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        _: &RenderExtensionContext,
    ) -> Option<RenderLayerScene> {
        self.layer(id, *rect, layer)
            .and_then(|value| value.scene.clone())
    }
    fn render_layer_items_with_context(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        _: &RenderDamage,
        layer: RenderExtensionLayer,
        _: &RenderExtensionContext,
    ) -> Option<Vec<RenderLayerItem>> {
        self.layer(id, *rect, layer)
            .and_then(|value| value.items.clone())
    }
    fn render_layer_ops_with_context(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        _: &RenderDamage,
        layer: RenderExtensionLayer,
        _: &RenderExtensionContext,
    ) -> Option<Vec<RenderOp>> {
        self.layer(id, *rect, layer)
            .and_then(|value| value.ops.clone())
    }
    fn render_surface(
        &self,
        _: &mut dyn std::io::Write,
        _: Uuid,
        _: &ExtensionRect,
        _: &RenderDamage,
    ) -> std::io::Result<bool> {
        bmux_attach_pipeline::projection::record(
            bmux_attach_pipeline::projection::PaintOperation::Rejected(
                bmux_attach_pipeline::projection::ProjectionFailure::Unsupported {
                    reason: "retained extension requested imperative rendering without neutral output",
                },
            ),
        );
        Err(std::io::Error::other(
            "imperative presentation is unsupported by retained output",
        ))
    }
}

/// Sample complete intent outside the synchronous composition transaction.
/// Existing producer registry remains private to the view; nothing global is replaced.
pub(super) fn refresh(
    sources: &[Arc<dyn AttachRenderExtension>],
    surfaces: &[(Uuid, ExtensionRect, RenderExtensionContext)],
) -> anyhow::Result<Vec<Arc<dyn AttachRenderExtension>>> {
    if sources.len() > 64 || surfaces.len() > 128 {
        anyhow::bail!("extension producer budget exceeded");
    }
    let mut retained: Vec<Arc<dyn AttachRenderExtension>> = Vec::new();
    let mut entries = 0_usize;
    for source in sources {
        source.refresh_state();
        let mut values = Vec::new();
        for (id, rect, context) in surfaces {
            let mut layers = Vec::new();
            for layer in [
                RenderExtensionLayer::BeforePaneContent,
                RenderExtensionLayer::AfterPaneContent,
            ] {
                let scene = source.render_layer_scene_with_context(*id, rect, layer, context);
                let items = if scene.is_none() {
                    source.render_layer_items_with_context(
                        *id,
                        rect,
                        &RenderDamage::FullSurface,
                        layer,
                        context,
                    )
                } else {
                    None
                };
                let ops = if scene.is_none() && items.is_none() {
                    source.render_layer_ops_with_context(
                        *id,
                        rect,
                        &RenderDamage::FullSurface,
                        layer,
                        context,
                    )
                } else {
                    None
                };
                entries = entries
                    .saturating_add(scene.as_ref().map_or(0, |scene| scene.items.len()))
                    .saturating_add(items.as_ref().map_or(0, Vec::len))
                    .saturating_add(ops.as_ref().map_or(0, Vec::len));
                if entries > 131_072 {
                    anyhow::bail!("extension intent budget exceeded");
                }
                layers.push(Layer {
                    phase: layer,
                    scene,
                    items,
                    ops,
                    damage: source.surface_layer_damage(*id, rect, layer),
                    revision: source.render_layer_revision(*id, layer),
                });
            }
            values.push(Surface {
                id: *id,
                rect: *rect,
                content_override: source.content_rect_override(*id),
                under_cells: source.render_before_content_cells_with_context(
                    *id,
                    rect,
                    &RenderDamage::FullSurface,
                    context,
                ),
                chrome: source.surface_chrome_with_context(*id, rect, context),
                layers,
            });
        }
        retained.push(Arc::new(RetainedExtension {
            name: source.name().to_owned(),
            surfaces: values,
        }));
    }
    Ok(retained)
}
