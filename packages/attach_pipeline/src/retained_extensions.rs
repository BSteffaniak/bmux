//! Render extensions are sampled by the presentation producer, not by output composition.
//!
//! Before each frame, [`RetainedExtensions::refresh`] asks every extension for its complete intent
//! on every surface, and frame planning sees only those immutable, retained values.
//!
//! A layer whose extension reports the same `Some` revision as last frame, for the same surface
//! rect and context, is not sampled again: its previous sample is reused as is (see
//! [`AttachRenderExtension::render_layer_revision`]), down to the scene's allocation, which the
//! renderer recognises and neither converts nor diffs again. Unchanged decorations thus cost
//! nothing per frame beyond the revision queries.
use bmux_plugin::{
    AttachRenderExtension, ExtensionRect, RenderExtensionContext, RenderExtensionLayer,
    RenderLayerScene, RenderSurfaceChrome,
};
use std::sync::Arc;
use uuid::Uuid;

/// Most producers sampled per frame.
const MAX_SOURCES: usize = 64;
/// Most surfaces sampled per frame.
const MAX_SURFACES: usize = 128;
/// Most scene items sampled per frame, across producers.
const MAX_ENTRIES: usize = 131_072;

const LAYERS: [RenderExtensionLayer; 2] = [
    RenderExtensionLayer::BeforePaneContent,
    RenderExtensionLayer::AfterPaneContent,
];

/// One layer of one surface as sampled from its extension.
struct Layer {
    revision: Option<u64>,
    scene: Option<Arc<RenderLayerScene>>,
    /// The cells the layer paints, worked out once when sampled.
    painted: Vec<ExtensionRect>,
}

impl Layer {
    fn entries(&self) -> usize {
        self.scene.as_ref().map_or(0, |scene| scene.items.len())
    }
}

/// One surface as sampled from an extension: where and how it was asked, and what it answered.
struct Surface {
    id: Uuid,
    rect: ExtensionRect,
    context: RenderExtensionContext,
    chrome: RenderSurfaceChrome,
    content_override: Option<ExtensionRect>,
    /// Indexed by [`layer_index`].
    layers: [Arc<Layer>; 2],
}

const fn layer_index(layer: RenderExtensionLayer) -> usize {
    match layer {
        RenderExtensionLayer::BeforePaneContent => 0,
        RenderExtensionLayer::AfterPaneContent => 1,
    }
}

struct RetainedExtension {
    name: String,
    surfaces: Vec<Arc<Surface>>,
}
impl RetainedExtension {
    fn surface(&self, id: Uuid, rect: ExtensionRect) -> Option<&Surface> {
        self.surfaces
            .iter()
            .find(|surface| {
                // Intent is sampled in authoritative geometry. Composition may use
                // a viewport-clipped subrectangle; lowering still owns clipping, so the
                // clipped rect must still find the surface's scene.
                surface.id == id
                    && rect.x >= surface.rect.x
                    && rect.y >= surface.rect.y
                    && u32::from(rect.x) + u32::from(rect.w)
                        <= u32::from(surface.rect.x) + u32::from(surface.rect.w)
                    && u32::from(rect.y) + u32::from(rect.h)
                        <= u32::from(surface.rect.y) + u32::from(surface.rect.h)
            })
            .map(AsRef::as_ref)
    }
    fn layer(&self, id: Uuid, rect: ExtensionRect, layer: RenderExtensionLayer) -> Option<&Layer> {
        self.surface(id, rect)
            .map(|surface| surface.layers[layer_index(layer)].as_ref())
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
    fn content_rect_override(&self, id: Uuid) -> Option<ExtensionRect> {
        self.surfaces
            .iter()
            .find(|surface| surface.id == id)
            .and_then(|surface| surface.content_override)
    }
    fn paints_inside_content(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        content: &ExtensionRect,
    ) -> bool {
        self.layer(id, *rect, layer).is_some_and(|value| {
            value
                .painted
                .iter()
                .any(|painted| painted.intersects(*content))
        })
    }
    fn render_layer_revision(&self, id: Uuid, layer: RenderExtensionLayer) -> Option<u64> {
        self.surfaces
            .iter()
            .find(|surface| surface.id == id)
            .and_then(|surface| surface.layers[layer_index(layer)].revision)
    }
    fn render_layer_scene_with_context(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<RenderLayerScene> {
        self.render_layer_scene_shared(id, rect, layer, context)
            .map(|scene| scene.as_ref().clone())
    }
    fn render_layer_scene_shared(
        &self,
        id: Uuid,
        rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        _: &RenderExtensionContext,
    ) -> Option<Arc<RenderLayerScene>> {
        self.layer(id, *rect, layer)
            .and_then(|value| value.scene.clone())
    }
}

/// What one producer answered last frame.
struct Sampled {
    source: Arc<dyn AttachRenderExtension>,
    retained: Arc<RetainedExtension>,
}

/// Samples render extensions' retained intent before each frame, reusing what an extension's
/// unchanged revisions say has not changed.
#[derive(Default)]
pub struct RetainedExtensions {
    sampled: Vec<Sampled>,
    prepared: Option<Arc<[Arc<dyn AttachRenderExtension>]>>,
    /// Producers being sampled, and the surfaces of the one being sampled (kept to reuse their
    /// allocations).
    next: Vec<Sampled>,
    scratch: Vec<Arc<Surface>>,
}

impl RetainedExtensions {
    /// Sample complete intent outside the synchronous composition transaction: every producer of
    /// `sources` on every one of `surfaces` (its id, authoritative rect and context). Returns the
    /// immutable snapshots frame planning renders from, one per producer and in their order;
    /// when nothing changed, the very ones returned last time.
    ///
    /// # Errors
    ///
    /// When there are too many producers or surfaces, or they answer with too much intent. What
    /// was sampled before is then kept for the next frame.
    pub fn refresh(
        &mut self,
        sources: &[Arc<dyn AttachRenderExtension>],
        surfaces: &[(Uuid, ExtensionRect, RenderExtensionContext)],
    ) -> anyhow::Result<Arc<[Arc<dyn AttachRenderExtension>]>> {
        if sources.len() > MAX_SOURCES || surfaces.len() > MAX_SURFACES {
            anyhow::bail!("extension producer budget exceeded");
        }
        let mut entries = 0_usize;
        self.next.clear();
        let mut unchanged = self.sampled.len() == sources.len();
        for (index, source) in sources.iter().enumerate() {
            source.refresh_state();
            let previous = self
                .sampled
                .iter()
                .find(|sampled| Arc::ptr_eq(&sampled.source, source))
                .map(|sampled| &sampled.retained);
            let retained =
                sample_source(&mut self.scratch, source, previous, surfaces, &mut entries)?;
            unchanged = unchanged
                && self.sampled.get(index).is_some_and(|last| {
                    Arc::ptr_eq(&last.source, source) && Arc::ptr_eq(&last.retained, &retained)
                });
            self.next.push(Sampled {
                source: source.clone(),
                retained,
            });
        }
        if unchanged && let Some(prepared) = &self.prepared {
            self.next.clear();
            return Ok(prepared.clone());
        }
        std::mem::swap(&mut self.sampled, &mut self.next);
        self.next.clear();
        let prepared = self
            .sampled
            .iter()
            .map(|sampled| sampled.retained.clone() as Arc<dyn AttachRenderExtension>)
            .collect::<Arc<[_]>>();
        self.prepared = Some(prepared.clone());
        Ok(prepared)
    }
}

/// Sample one producer on every surface, reusing `previous` (its last sample) wherever its
/// revisions allow; returns `previous` itself when nothing changed. `scratch` holds the surfaces
/// sampled.
fn sample_source(
    scratch: &mut Vec<Arc<Surface>>,
    source: &Arc<dyn AttachRenderExtension>,
    previous: Option<&Arc<RetainedExtension>>,
    surfaces: &[(Uuid, ExtensionRect, RenderExtensionContext)],
    entries: &mut usize,
) -> anyhow::Result<Arc<RetainedExtension>> {
    scratch.clear();
    let mut unchanged = previous.is_some_and(|previous| previous.surfaces.len() == surfaces.len());
    for (index, (id, rect, context)) in surfaces.iter().enumerate() {
        let last = previous.and_then(|previous| {
            previous.surfaces.iter().find(|surface| {
                surface.id == *id && surface.rect == *rect && surface.context == *context
            })
        });
        let surface = sample_surface(source.as_ref(), last, *id, *rect, context, entries)?;
        unchanged = unchanged
            && previous
                .and_then(|previous| previous.surfaces.get(index))
                .is_some_and(|last| Arc::ptr_eq(last, &surface));
        scratch.push(surface);
    }
    if unchanged && let Some(previous) = previous {
        return Ok(previous.clone());
    }
    Ok(Arc::new(RetainedExtension {
        name: source.name().to_owned(),
        surfaces: scratch.clone(),
    }))
}

/// Sample `source` on one surface, reusing `last` (this surface's previous sample, asked with the
/// same rect and context) where the layers' revisions are unchanged.
fn sample_surface(
    source: &dyn AttachRenderExtension,
    last: Option<&Arc<Surface>>,
    id: Uuid,
    rect: ExtensionRect,
    context: &RenderExtensionContext,
    entries: &mut usize,
) -> anyhow::Result<Arc<Surface>> {
    let layers = [
        sample_layer(source, last, id, rect, LAYERS[0], context, entries)?,
        sample_layer(source, last, id, rect, LAYERS[1], context, entries)?,
    ];
    if let Some(last) = last
        && layers
            .iter()
            .zip(&last.layers)
            .all(|(layer, last)| Arc::ptr_eq(layer, last))
    {
        return Ok(last.clone());
    }
    Ok(Arc::new(Surface {
        id,
        rect,
        context: context.clone(),
        content_override: source.content_rect_override(id),
        chrome: source.surface_chrome_with_context(id, &rect, context),
        layers,
    }))
}

fn sample_layer(
    source: &dyn AttachRenderExtension,
    last: Option<&Arc<Surface>>,
    id: Uuid,
    rect: ExtensionRect,
    layer: RenderExtensionLayer,
    context: &RenderExtensionContext,
    entries: &mut usize,
) -> anyhow::Result<Arc<Layer>> {
    let revision = source.render_layer_revision(id, layer);
    let last = last.map(|surface| &surface.layers[layer_index(layer)]);
    let sampled = match last {
        Some(last) if revision.is_some() && last.revision == revision => last.clone(),
        _ => {
            let scene = source.render_layer_scene_shared(id, &rect, layer, context);
            let painted = scene
                .iter()
                .flat_map(|scene| &scene.items)
                .flat_map(bmux_plugin::RenderSceneItem::painted_rects)
                .collect();
            Arc::new(Layer {
                revision,
                scene,
                painted,
            })
        }
    };
    *entries = entries.saturating_add(sampled.entries());
    if *entries > MAX_ENTRIES {
        anyhow::bail!("extension intent budget exceeded");
    }
    Ok(sampled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{RenderSceneItem, RenderStyle};

    #[derive(Clone, Copy)]
    enum Representation {
        Scene,
        EmptyScene,
        NoScene,
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
                Representation::NoScene => None,
            }
        }
    }

    fn snapshot(representation: Representation) -> Arc<dyn AttachRenderExtension> {
        let sources: Vec<Arc<dyn AttachRenderExtension>> = vec![Arc::new(Source(representation))];
        RetainedExtensions::default()
            .refresh(
                &sources,
                &[(
                    Uuid::from_u128(1),
                    ExtensionRect::new(2, 3, 80, 24),
                    RenderExtensionContext::default(),
                )],
            )
            .expect("sample intent")[0]
            .clone()
    }

    #[test]
    fn viewport_clipping_preserves_retained_scene() {
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
        }
    }

    /// An empty scene is retained as one; an extension without a scene paints nothing.
    #[test]
    fn empty_and_absent_scenes_are_kept_apart() {
        let id = Uuid::from_u128(1);
        let rect = ExtensionRect::new(2, 3, 38, 17);
        let context = RenderExtensionContext::default();
        let layer = RenderExtensionLayer::AfterPaneContent;
        let scene = snapshot(Representation::EmptyScene)
            .render_layer_scene_with_context(id, &rect, layer, &context)
            .expect("empty scene is a supported representation");
        assert_eq!(scene.items, [] as [bmux_plugin::RenderSceneItem; 0]);
        assert!(
            snapshot(Representation::NoScene)
                .render_layer_scene_with_context(id, &rect, layer, &context)
                .is_none()
        );
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
        let error = crate::terminal::write_frame(&mut BackpressuredTerminal, b"frame")
            .expect_err("blocked consumer is a delivery error, not successful output");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        let after = retained
            .render_layer_scene_with_context(id, &rect, layer, &context)
            .expect("blocked delivery must keep the retained scene");
        assert_eq!(before.items, after.items);
        assert_eq!(before.revision, after.revision);
    }

    /// A producer counting how often it is sampled, whose scene is its `text` at `revision`.
    struct Counting {
        revision: std::sync::Mutex<(Option<u64>, &'static str)>,
        scenes: std::sync::atomic::AtomicUsize,
        surface_queries: std::sync::atomic::AtomicUsize,
    }

    impl Counting {
        fn new(revision: Option<u64>, text: &'static str) -> Arc<Self> {
            Arc::new(Self {
                revision: std::sync::Mutex::new((revision, text)),
                scenes: std::sync::atomic::AtomicUsize::new(0),
                surface_queries: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn set(&self, revision: Option<u64>, text: &'static str) {
            *self.revision.lock().expect("revision") = (revision, text);
        }

        fn scenes(&self) -> usize {
            self.scenes.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn surface_queries(&self) -> usize {
            self.surface_queries
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl AttachRenderExtension for Counting {
        fn name(&self) -> &'static str {
            "test.counting"
        }

        fn render_layer_revision(&self, _: Uuid, _: RenderExtensionLayer) -> Option<u64> {
            self.revision.lock().expect("revision").0
        }

        fn surface_chrome_with_context(
            &self,
            _: Uuid,
            _: &ExtensionRect,
            _: &RenderExtensionContext,
        ) -> RenderSurfaceChrome {
            self.surface_queries
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            RenderSurfaceChrome::Fallback
        }

        fn render_layer_scene_with_context(
            &self,
            _: Uuid,
            rect: &ExtensionRect,
            _: RenderExtensionLayer,
            _: &RenderExtensionContext,
        ) -> Option<RenderLayerScene> {
            self.scenes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (revision, text) = *self.revision.lock().expect("revision");
            Some(RenderLayerScene::new(
                revision,
                vec![RenderSceneItem::text(
                    "label",
                    0,
                    rect.x,
                    rect.y,
                    text,
                    RenderStyle::new(),
                )],
            ))
        }
    }

    const ID: Uuid = Uuid::from_u128(1);
    const LAYER: RenderExtensionLayer = RenderExtensionLayer::AfterPaneContent;

    fn surfaces(rect: ExtensionRect) -> [(Uuid, ExtensionRect, RenderExtensionContext); 1] {
        [(ID, rect, RenderExtensionContext::default())]
    }

    /// The scene the renderer reads from the sample, for `ID` at `rect`.
    fn scene(
        prepared: &[Arc<dyn AttachRenderExtension>],
        rect: ExtensionRect,
    ) -> Arc<RenderLayerScene> {
        prepared[0]
            .render_layer_scene_shared(ID, &rect, LAYER, &RenderExtensionContext::default())
            .expect("sampled scene")
    }

    fn text(scene: &RenderLayerScene) -> &str {
        match &scene.items[0].kind {
            bmux_plugin::RenderSceneItemKind::Text { text, .. } => text,
            other => panic!("unexpected item {other:?}"),
        }
    }

    #[test]
    fn an_unchanged_revision_reuses_the_sample_without_asking_again() {
        let source = Counting::new(Some(1), "kept");
        let sources: Vec<Arc<dyn AttachRenderExtension>> = vec![source.clone()];
        let rect = ExtensionRect::new(2, 3, 80, 24);
        let mut retained = RetainedExtensions::default();
        let first = retained.refresh(&sources, &surfaces(rect)).expect("sample");
        assert_eq!(source.scenes(), 2, "both layers are sampled once");
        let surface_queries = source.surface_queries();
        for _ in 0..3 {
            let next = retained.refresh(&sources, &surfaces(rect)).expect("sample");
            assert!(
                Arc::ptr_eq(&first, &next),
                "the same snapshots are returned"
            );
            // Still rendered from: the very scene, with its items.
            assert!(Arc::ptr_eq(&scene(&first, rect), &scene(&next, rect)));
            assert_eq!(text(&scene(&next, rect)), "kept");
        }
        assert_eq!(source.scenes(), 2, "an unchanged layer is not rebuilt");
        assert_eq!(source.surface_queries(), surface_queries);
    }

    #[test]
    fn a_revision_bump_rebuilds_the_layer() {
        let source = Counting::new(Some(1), "before");
        let sources: Vec<Arc<dyn AttachRenderExtension>> = vec![source.clone()];
        let rect = ExtensionRect::new(2, 3, 80, 24);
        let mut retained = RetainedExtensions::default();
        let first = retained.refresh(&sources, &surfaces(rect)).expect("sample");
        source.set(Some(2), "after");
        let next = retained.refresh(&sources, &surfaces(rect)).expect("sample");
        assert_eq!(source.scenes(), 4, "both layers are sampled again");
        assert!(!Arc::ptr_eq(&first, &next));
        assert_eq!(text(&scene(&first, rect)), "before");
        assert_eq!(text(&scene(&next, rect)), "after");
    }

    #[test]
    fn a_resized_surface_or_changed_context_is_sampled_again() {
        let source = Counting::new(Some(1), "kept");
        let sources: Vec<Arc<dyn AttachRenderExtension>> = vec![source.clone()];
        let rect = ExtensionRect::new(2, 3, 80, 24);
        let mut retained = RetainedExtensions::default();
        retained.refresh(&sources, &surfaces(rect)).expect("sample");
        let resized = ExtensionRect::new(2, 3, 60, 24);
        let next = retained
            .refresh(&sources, &surfaces(resized))
            .expect("sample");
        assert_eq!(source.scenes(), 4, "a resized surface is sampled again");
        assert_eq!(text(&scene(&next, resized)), "kept");
        let context = RenderExtensionContext {
            opaque_occluders: vec![ExtensionRect::new(0, 0, 4, 4)],
            ..RenderExtensionContext::default()
        };
        retained
            .refresh(&sources, &[(ID, resized, context)])
            .expect("sample");
        assert_eq!(source.scenes(), 6, "a changed context is sampled again");
    }

    #[test]
    fn without_a_revision_every_frame_is_sampled_again() {
        let source = Counting::new(None, "fresh");
        let sources: Vec<Arc<dyn AttachRenderExtension>> = vec![source.clone()];
        let rect = ExtensionRect::new(2, 3, 80, 24);
        let mut retained = RetainedExtensions::default();
        let first = retained.refresh(&sources, &surfaces(rect)).expect("sample");
        let next = retained.refresh(&sources, &surfaces(rect)).expect("sample");
        assert_eq!(source.scenes(), 4);
        assert!(!Arc::ptr_eq(&scene(&first, rect), &scene(&next, rect)));
    }
}
