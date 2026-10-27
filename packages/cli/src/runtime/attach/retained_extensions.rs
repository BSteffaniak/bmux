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
        self.surfaces.iter().find(|surface| {
            // Intent is sampled in authoritative geometry. Composition may use
            // a viewport-clipped subrectangle; lowering still owns clipping and
            // must not mistake that geometry for an imperative-only extension.
            surface.id == id
                && rect.x >= surface.rect.x
                && rect.y >= surface.rect.y
                && u32::from(rect.x) + u32::from(rect.w)
                    <= u32::from(surface.rect.x) + u32::from(surface.rect.w)
                && u32::from(rect.y) + u32::from(rect.h)
                    <= u32::from(surface.rect.y) + u32::from(surface.rect.h)
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{RenderSceneItem, RenderStyle};

    #[derive(Clone, Copy)]
    enum Representation {
        Scene,
        EmptyScene,
        Items,
        EmptyOps,
        ImperativeOnly,
    }

    struct Source(Representation);

    impl AttachRenderExtension for Source {
        fn name(&self) -> &'static str {
            "test.retained"
        }

        fn render_layer_scene_with_context(
            &self,
            _: Uuid,
            _: &ExtensionRect,
            _: RenderExtensionLayer,
            _: &RenderExtensionContext,
        ) -> Option<RenderLayerScene> {
            match self.0 {
                Representation::Scene => Some(RenderLayerScene::new(
                    Some(7),
                    vec![RenderSceneItem::text(
                        "label",
                        0,
                        2,
                        3,
                        "kept",
                        RenderStyle::new(),
                    )],
                )),
                Representation::EmptyScene => Some(RenderLayerScene::new(Some(7), Vec::new())),
                _ => None,
            }
        }

        fn render_layer_items_with_context(
            &self,
            _: Uuid,
            _: &ExtensionRect,
            _: &RenderDamage,
            _: RenderExtensionLayer,
            _: &RenderExtensionContext,
        ) -> Option<Vec<RenderLayerItem>> {
            match self.0 {
                Representation::Scene | Representation::EmptyScene => {
                    panic!("a selected retained scene must not probe lower representations")
                }
                Representation::Items => Some(vec![RenderLayerItem::Op(RenderOp::text_run(
                    2,
                    3,
                    "kept",
                    RenderStyle::new(),
                ))]),
                _ => None,
            }
        }

        fn render_layer_ops_with_context(
            &self,
            _: Uuid,
            _: &ExtensionRect,
            _: &RenderDamage,
            _: RenderExtensionLayer,
            _: &RenderExtensionContext,
        ) -> Option<Vec<RenderOp>> {
            match self.0 {
                Representation::EmptyOps => Some(Vec::new()),
                Representation::ImperativeOnly => None,
                _ => panic!("a selected representation must not probe operations"),
            }
        }

        fn render_surface(
            &self,
            _: &mut dyn std::io::Write,
            _: Uuid,
            _: &ExtensionRect,
            _: &RenderDamage,
        ) -> std::io::Result<bool> {
            panic!("sampling must never invoke imperative drawing")
        }
    }

    fn snapshot(representation: Representation) -> Arc<dyn AttachRenderExtension> {
        let sources: Vec<Arc<dyn AttachRenderExtension>> = vec![Arc::new(Source(representation))];
        refresh(
            &sources,
            &[(
                Uuid::from_u128(1),
                ExtensionRect::new(2, 3, 80, 24),
                RenderExtensionContext::default(),
            )],
        )
        .expect("sample intent")
        .remove(0)
    }

    #[test]
    fn viewport_clipping_preserves_retained_scene_and_items() {
        let id = Uuid::from_u128(1);
        let rect = ExtensionRect::new(2, 3, 38, 17);
        let context = RenderExtensionContext::default();
        for layer in [
            RenderExtensionLayer::BeforePaneContent,
            RenderExtensionLayer::AfterPaneContent,
        ] {
            let retained = snapshot(Representation::Scene);
            let scene = retained
                .render_layer_scene_with_context(id, &rect, layer, &context)
                .expect("clipped surface must retain its scene representation");
            assert_eq!(scene.revision, Some(7));
            assert_eq!(scene.items.len(), 1);
            assert_eq!(
                scene.items[0],
                RenderSceneItem::text("label", 0, 2, 3, "kept", RenderStyle::new()),
            );

            let retained = snapshot(Representation::Items);
            assert!(
                retained
                    .render_layer_scene_with_context(id, &rect, layer, &context)
                    .is_none()
            );
            let items = retained
                .render_layer_items_with_context(
                    id,
                    &rect,
                    &RenderDamage::FullSurface,
                    layer,
                    &context,
                )
                .expect("clipped surface must retain its item representation");
            assert_eq!(items.len(), 1);
        }
    }

    #[test]
    fn empty_retained_output_does_not_select_imperative_fallback() {
        let id = Uuid::from_u128(1);
        let rect = ExtensionRect::new(2, 3, 38, 17);
        let context = RenderExtensionContext::default();
        let layer = RenderExtensionLayer::AfterPaneContent;
        let scene = snapshot(Representation::EmptyScene)
            .render_layer_scene_with_context(id, &rect, layer, &context)
            .expect("empty scene is a supported representation");
        assert_eq!(scene.items, [] as [bmux_plugin::RenderSceneItem; 0]);
        let ops = snapshot(Representation::EmptyOps)
            .render_layer_ops_with_context(id, &rect, &RenderDamage::FullSurface, layer, &context)
            .expect("empty operations are a supported no-op");
        assert_eq!(ops, [] as [bmux_plugin::RenderOp; 0]);
    }

    #[test]
    fn unsampled_surface_or_geometry_does_not_reuse_retained_intent() {
        let retained = snapshot(Representation::Scene);
        for (id, rect) in [
            (Uuid::from_u128(2), ExtensionRect::new(2, 3, 80, 24)),
            (Uuid::from_u128(1), ExtensionRect::new(1, 3, 80, 24)),
            (Uuid::from_u128(1), ExtensionRect::new(2, 3, 81, 24)),
        ] {
            assert!(
                retained
                    .render_layer_scene_with_context(
                        id,
                        &rect,
                        RenderExtensionLayer::AfterPaneContent,
                        &RenderExtensionContext::default(),
                    )
                    .is_none(),
                "unsampled geometry must not reuse another surface's intent"
            );
        }
    }

    #[test]
    fn terminal_backpressure_does_not_change_retained_representation() {
        struct BackpressuredTerminal;
        impl std::io::Write for BackpressuredTerminal {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::WouldBlock.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                panic!("failed delivery must not flush or claim success")
            }
        }

        let retained = snapshot(Representation::Scene);
        let id = Uuid::from_u128(1);
        let rect = ExtensionRect::new(2, 3, 38, 17);
        let context = RenderExtensionContext::default();
        let layer = RenderExtensionLayer::AfterPaneContent;
        let before = retained
            .render_layer_scene_with_context(id, &rect, layer, &context)
            .expect("retained intent before blocked delivery");
        let error =
            bmux_attach_pipeline::terminal::write_frame(&mut BackpressuredTerminal, b"frame")
                .expect_err("blocked consumer is a delivery error, not successful output");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        let after = retained
            .render_layer_scene_with_context(id, &rect, layer, &context)
            .expect("blocked delivery must not select imperative rendering");
        assert_eq!(before.items, after.items);
        assert_eq!(before.revision, after.revision);
    }

    #[test]
    fn imperative_only_output_remains_rejected_without_touching_terminal() {
        struct BlockedTerminal;
        impl std::io::Write for BlockedTerminal {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                panic!("representation rejection must not wait for a slow terminal")
            }
            fn flush(&mut self) -> std::io::Result<()> {
                panic!("representation rejection must not flush a slow terminal")
            }
        }

        let retained = snapshot(Representation::ImperativeOnly);
        let id = Uuid::from_u128(1);
        let rect = ExtensionRect::new(2, 3, 38, 17);
        let context = RenderExtensionContext::default();
        let layer = RenderExtensionLayer::AfterPaneContent;
        assert!(
            retained
                .render_layer_scene_with_context(id, &rect, layer, &context)
                .is_none()
        );
        assert!(
            retained
                .render_layer_ops_with_context(
                    id,
                    &rect,
                    &RenderDamage::FullSurface,
                    layer,
                    &context
                )
                .is_none()
        );
        let error = retained
            .render_surface(&mut BlockedTerminal, id, &rect, &RenderDamage::FullSurface)
            .expect_err("unsupported output must not be silently treated as a no-op");
        assert_eq!(
            error.to_string(),
            "imperative presentation is unsupported by retained output"
        );
    }
}
