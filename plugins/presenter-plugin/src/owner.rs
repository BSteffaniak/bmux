//! Canonical owner for application-supplied trusted linked components.
use bmux_presenter::custom::{
    CompatibilityKey, CustomComponentFactory, LinkedComponentFactory, LinkedRegistry, LinkedState,
};
use bmux_presenter::{Block, BlockKind, Deck};
use bmux_presenter_runtime::mirror::MirrorFrame;
use bmux_presenter_runtime::{HostError, Owner, OwnerContext, OwnerUpdate, SurfaceInput};
use bmux_tui::prelude::{
    Component, Constraints, Element, Event, EventCx, EventOutcome, LayoutCx, LayoutNode, PaintCx,
    Size, TextBlock,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

type InstanceState = BTreeMap<CompatibilityKey, LinkedState>;
struct Geometry {
    layout: LayoutNode,
    slide_id: String,
    reveal_step: u32,
    deck_revision: u64,
    interaction_revision: u64,
    control_scene: Option<bmux_presenter_runtime::mirror::MirrorPrivateScene>,
    launcher: Option<bmux_tui::geometry::Rect>,
}
/// Canonical fixed-canvas owner for trusted, application-registered components.
/// Shared and independent host instance identities select separate state maps.
/// Reload retains only matching block ID/component type/state-version keys.
pub struct LinkedOwner {
    deck: Deck,
    rich: crate::rich::RichAssets,
    linked: LinkedRegistry,
    instances: BTreeMap<String, InstanceState>,
    interactions: BTreeMap<(String, String), InstanceState>,
    layouts: BTreeMap<(String, String, String), Geometry>,
    revisions: BTreeMap<String, u64>,
    demo_environment: Option<crate::playbook_live::DemoEnvironment>,
    demos: BTreeMap<String, crate::playbook_live::LiveDemos>,
}
impl LinkedOwner {
    /// Prepare a trusted linked owner with an application-supplied registry.
    /// Assets must already be authorized by the caller; preparation performs no
    /// network access and does not execute component effects.
    pub fn new(
        deck: Deck,
        assets: &[bmux_presenter_runtime::Asset],
        linked: LinkedRegistry,
    ) -> Result<Self, HostError> {
        let mut rich = crate::rich::RichAssets::prepare(assets).map_err(|_| HostError::Invalid)?;
        rich.code = std::sync::Arc::new(crate::rich::highlight(&deck));
        Ok(Self::with_registry(deck, rich, linked))
    }
    #[cfg(test)]
    fn with_examples(deck: Deck, rich: crate::rich::RichAssets) -> Self {
        Self::with_registry(deck, rich, crate::demo::linked_registry())
    }
    fn with_registry(deck: Deck, rich: crate::rich::RichAssets, linked: LinkedRegistry) -> Self {
        Self {
            deck,
            rich,
            linked,
            instances: BTreeMap::new(),
            interactions: BTreeMap::new(),
            layouts: BTreeMap::new(),
            revisions: BTreeMap::new(),
            demo_environment: None,
            demos: BTreeMap::new(),
        }
    }
    /// Detached candidate: failed preparation cannot alter the live owner or its mutexes.
    fn prepare_candidate(
        &self,
        deck: Deck,
        rich: crate::rich::RichAssets,
    ) -> Result<Self, HostError> {
        let mut candidate = Self::with_registry(deck, rich, self.linked.clone());
        for (instance, states) in &self.instances {
            let mut retained = BTreeMap::new();
            for slide in &candidate.deck.slides {
                for block in custom_blocks(&slide.blocks, u32::MAX) {
                    let Some(key) = candidate.linked.compatibility_key(block) else {
                        continue;
                    };
                    if let Some(state) = states.get(&key) {
                        let value = state.lock().map_err(|_| HostError::Invalid)?.clone();
                        retained.insert(key, Arc::new(Mutex::new(value)));
                    }
                }
            }
            candidate.instances.insert(instance.clone(), retained);
        }
        candidate.revisions.clone_from(&self.revisions);
        Ok(candidate)
    }
    pub(crate) fn configure_demos(
        &mut self,
        environment: crate::playbook_live::DemoEnvironment,
    ) -> Result<(), HostError> {
        let demos = crate::playbook_live::LiveDemos::prepare(&self.deck, &environment)
            .map_err(|_| HostError::Invalid)?;
        self.demos.insert("shared".into(), demos);
        self.demo_environment = Some(environment);
        Ok(())
    }
    pub(crate) fn scenes(
        &mut self,
        context: OwnerContext<'_>,
    ) -> Result<Vec<MirrorFrame>, HostError> {
        let Some(slide) = context.state.current_slide(&self.deck) else {
            return Ok(vec![]);
        };
        let blocks = custom_blocks(&slide.blocks, context.state.reveal_step);
        let states = self
            .instances
            .entry(context.instance.to_owned())
            .or_default();
        for block in custom_blocks(&slide.blocks, u32::MAX) {
            let Some(key) = self.linked.compatibility_key(block) else {
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(entry) = states.entry(key) {
                let factory = self
                    .linked
                    .factory(&entry.key().component_type)
                    .ok_or(HostError::Invalid)?;
                let initial = factory
                    .initial_state(block)
                    .map_err(|_| HostError::Invalid)?;
                entry.insert(Arc::new(Mutex::new(initial)));
            }
        }
        if self.interactions.len() >= 256
            && !self
                .interactions
                .contains_key(&(context.participant.to_owned(), context.instance.to_owned()))
        {
            return Err(HostError::Limit);
        }
        let interactions = self
            .interactions
            .entry((context.participant.to_owned(), context.instance.to_owned()))
            .or_default();
        interactions.retain(|key, _| states.contains_key(key));
        for key in states.keys() {
            interactions
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(serde_json::json!({}))));
        }
        let focused_registry = render_registry(&self.linked, states, None, Some(interactions))?;
        let registry = render_registry(&self.linked, states, None, None)?;
        for block in &blocks {
            if self.linked.compatibility_key(block).is_some() {
                registry.build(block).map_err(|_| HostError::Invalid)?;
            }
        }
        let theme = bmux_presenter::PresentationTheme::for_deck(&self.deck);
        let component = bmux_presenter::SlideComponent::new(
            slide,
            context.state.reveal_step,
            &theme,
            &registry,
        )
        .rich_content(&self.rich);
        let canvas_size = self
            .demos
            .get(context.instance)
            .and_then(|demos| demos.visible(slide.id.as_str()))
            .map_or(Size::new(80, 38), |(_, demo)| {
                let area = demo.snapshot.surface.as_ref().map_or(
                    bmux_tui::geometry::Rect::new(
                        0,
                        0,
                        demo.preview_size.width,
                        demo.preview_size.height,
                    ),
                    |capture| capture.surface.buffer().area(),
                );
                Size::new(
                    area.width.saturating_add(2).clamp(80, 240),
                    area.height.saturating_add(8).clamp(38, 128),
                )
            });
        let mut buffer = bmux_tui::buffer::Buffer::empty(bmux_tui::geometry::Rect::new(
            0,
            0,
            canvas_size.width,
            canvas_size.height,
        ));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        let overlay = self
            .demos
            .get(context.instance)
            .and_then(|demos| demos.visible(slide.id.as_str()));
        let layout = if let Some((id, demo)) = overlay {
            let overlay = bmux_tui::composition::Overlay::new(component, demo.view(true))
                .id(format!("playbook-overlay-{id}"))
                .modal(bmux_tui::focus::FocusScopeId::new(format!("playbook-{id}")));
            let layout = overlay.layout(Constraints::tight(canvas_size), &mut LayoutCx::default());
            overlay.paint(&layout, &mut PaintCx::new(&mut frame));
            layout
        } else {
            let layout =
                component.layout(Constraints::tight(canvas_size), &mut LayoutCx::default());
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            layout
        };
        let mut images = bmux_tui::image_scene::ImageScene::default();
        images.reconcile(frame.images());
        let revision = self
            .revisions
            .get(context.instance)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(HostError::Limit)?;
        let mut private_scene = None;
        if let Some((id, demo)) = overlay {
            let operator =
                bmux_tui::composition::Overlay::new(TextBlock::new(""), demo.view(false))
                    .modal(bmux_tui::focus::FocusScopeId::new(format!("playbook-{id}")));
            let operator_size = canvas_size;
            let operator_layout =
                operator.layout(Constraints::tight(operator_size), &mut LayoutCx::default());
            let mut operator_buffer = bmux_tui::buffer::Buffer::empty(
                bmux_tui::geometry::Rect::new(0, 0, operator_size.width, operator_size.height),
            );
            let mut operator_frame = bmux_tui::frame::Frame::new(&mut operator_buffer);
            operator.paint(&operator_layout, &mut PaintCx::new(&mut operator_frame));
            let interactions = operator_frame
                .hits()
                .regions()
                .iter()
                .filter_map(|region| {
                    let input = crate::playbook_component::target_input(region.id.as_str())?;
                    Some(bmux_presenter_runtime::mirror::MirrorInteraction {
                        id: region.id.as_str().into(),
                        input: input.into(),
                        enabled: region.enabled,
                        area: bmux_presenter_runtime::mirror::CanvasRect {
                            x: region.area.x,
                            y: region.area.y,
                            width: region.area.width,
                            height: region.area.height,
                        },
                    })
                })
                .collect();
            let mut operator_images = bmux_tui::image_scene::ImageScene::default();
            operator_images.reconcile(operator_frame.images());
            private_scene = Some(bmux_presenter_runtime::mirror::MirrorPrivateScene {
                frame: Box::new(MirrorFrame::capture(
                    id.into(),
                    slide.id.as_str().into(),
                    revision,
                    &operator_frame,
                    &operator_images,
                    16_384,
                    2 * 1024 * 1024,
                )?),
                interactions,
                identity: serde_json::json!({"id":id,"owner_epoch":demo.snapshot.owner_epoch.to_string(),
                    "generation":demo.snapshot.attempt.as_ref().map(|attempt| attempt.generation),
                    "paused":demo.snapshot.attempt.as_ref().map(|attempt| attempt.paused),
                    "phase":demo.snapshot.attempt.as_ref().map(|attempt| attempt.phase.as_str())}),
            });
            if let Some(scene) = &mut private_scene
                && let Some(previous) = self.layouts.get(&(
                    context.participant.to_owned(),
                    context.instance.to_owned(),
                    id.to_owned(),
                ))
                && previous.deck_revision == context.cursor.deck_revision
                && previous.slide_id == slide.id.as_str()
                && previous.reveal_step == context.state.reveal_step
                && let Some(old) = &previous.control_scene
                && old.identity == scene.identity
                && old.frame.width == scene.frame.width
                && old.frame.height == scene.frame.height
                && serde_json::to_vec(&old.interactions).ok()
                    == serde_json::to_vec(&scene.interactions).ok()
            {
                // Output updates do not invalidate an unchanged authoritative
                // control target. Lifecycle/control transitions always do.
                scene.frame.revision = old.frame.revision;
            }
        }
        let mut surfaces = Vec::new();
        let mut geometries = BTreeMap::new();
        for block in blocks {
            if self.linked.compatibility_key(block).is_none() {
                continue;
            }
            let id = block.id.as_str().to_owned();
            let mut surface = MirrorFrame::capture(
                id.clone(),
                slide.id.as_str().into(),
                revision,
                &frame,
                &images,
                16_384,
                2 * 1024 * 1024,
            )?;
            if overlay.is_some_and(|(active, _)| active == id) {
                surface.private_scene = private_scene.clone();
                if let Some((_, demo)) = overlay {
                    surface.playbook = Some(
                        demo.audience_playbook(&context.cursor.run_id, revision)
                            .map_err(|_| HostError::Invalid)?,
                    );
                }
            }
            surface.focusable = registry.focusable(block);
            if surface.focusable && overlay.is_none() {
                // This is an immutable alternative projection, not shared focus state.
                // Every viewer selects it independently only after committing focus.
                let focused_component = bmux_presenter::SlideComponent::new(
                    slide,
                    context.state.reveal_step,
                    &theme,
                    &focused_registry,
                )
                .rich_content(&self.rich)
                .focused_block(&block.id);
                let mut focused_buffer = bmux_tui::buffer::Buffer::empty(
                    bmux_tui::geometry::Rect::new(0, 0, canvas_size.width, canvas_size.height),
                );
                let mut focused_frame = bmux_tui::frame::Frame::new(&mut focused_buffer);
                let focused_layout = focused_component
                    .layout(Constraints::tight(canvas_size), &mut LayoutCx::default());
                if !same_geometry(&layout, &focused_layout) {
                    return Err(HostError::Invalid);
                }
                focused_component.paint(&layout, &mut PaintCx::new(&mut focused_frame));
                let mut focused_images = bmux_tui::image_scene::ImageScene::default();
                focused_images.reconcile(focused_frame.images());
                let mut focused = MirrorFrame::capture(
                    id.clone(),
                    slide.id.as_str().into(),
                    revision,
                    &focused_frame,
                    &focused_images,
                    16_384,
                    2 * 1024 * 1024,
                )?;
                focused.focusable = true;
                surface.focused = Some(Box::new(focused));
            }
            let interaction_revision = self
                .layouts
                .get(&(
                    context.participant.to_owned(),
                    context.instance.to_owned(),
                    id.clone(),
                ))
                .filter(|old| {
                    old.deck_revision == context.cursor.deck_revision
                        && old.slide_id == slide.id.as_str()
                        && old.reveal_step == context.state.reveal_step
                        && old.control_scene.is_none()
                        && private_scene.is_none()
                })
                .map_or(revision, |old| old.interaction_revision);
            surface.interaction_revision = Some(interaction_revision);
            if let Some(focused) = &mut surface.focused {
                focused.interaction_revision = Some(interaction_revision);
            }
            surface.validate(16_384, 2 * 1024 * 1024)?;
            surfaces.push(surface);
            geometries.insert(
                (
                    context.participant.to_owned(),
                    context.instance.to_owned(),
                    id,
                ),
                Geometry {
                    layout: layout.clone(),
                    slide_id: slide.id.as_str().into(),
                    reveal_step: context.state.reveal_step,
                    deck_revision: context.cursor.deck_revision,
                    interaction_revision,
                    control_scene: private_scene.clone(),
                    launcher: frame
                        .hits()
                        .regions()
                        .iter()
                        .find(|hit| hit.id.as_str() == format!("{}/launch", block.id.as_str()))
                        .map(|hit| hit.area),
                },
            );
        }
        // Publish derived geometry only after every frame capture succeeds.
        self.layouts.retain(|(participant, instance, _), _| {
            participant != context.participant || instance != context.instance
        });
        self.layouts.extend(geometries);
        self.revisions.insert(context.instance.to_owned(), revision);
        Ok(surfaces)
    }
}
fn same_geometry(left: &LayoutNode, right: &LayoutNode) -> bool {
    left.id == right.id
        && left.size == right.size
        && left.children.len() == right.children.len()
        && left
            .children
            .iter()
            .zip(&right.children)
            .all(|(left, right)| {
                left.x == right.x && left.y == right.y && same_geometry(&left.node, &right.node)
            })
}
fn custom_blocks(blocks: &[Block], reveal: u32) -> Vec<&Block> {
    let mut found = Vec::new();
    for block in blocks {
        if block.reveal_step > reveal {
            continue;
        }
        match &block.kind {
            BlockKind::Custom { .. } => found.push(block),
            BlockKind::Columns { columns } => {
                for column in columns {
                    found.extend(custom_blocks(&column.blocks, reveal));
                }
            }
            _ => {}
        }
    }
    found
}
struct BoundFactory {
    factory: Arc<dyn LinkedComponentFactory>,
    states: BTreeMap<String, LinkedState>,
    interactions: BTreeMap<String, LinkedState>,
    active: Option<String>,
}
impl CustomComponentFactory for BoundFactory {
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String> {
        self.build_with_context(
            block,
            bmux_presenter::custom::PresentationBuildContext::default(),
        )
    }
    fn focusable(&self, block: &Block) -> bool {
        self.factory.focusable(block)
    }
    fn build_with_context<'a>(
        &'a self,
        block: &'a Block,
        mut context: bmux_presenter::custom::PresentationBuildContext,
    ) -> Result<Element<'a>, String> {
        let state = self
            .states
            .get(block.id.as_str())
            .ok_or("missing owner state")?;
        context.interaction = self.interactions.get(block.id.as_str()).cloned();
        Ok(Element::new(TargetComponent {
            child: self
                .factory
                .build_with_context(block, Arc::clone(state), context)?,
            active: self.active.as_deref() == Some(block.id.as_str()),
        }))
    }
}
struct TargetComponent<'a> {
    child: Element<'a>,
    active: bool,
}
impl Component for TargetComponent<'_> {
    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.child.revision()
    }
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.child.layout(constraints, cx)
    }
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.child.paint(layout, cx);
    }
    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        if !self.active
            || cx
                .find_rect(&layout.id)
                .is_none_or(|rect| rect.width == 0 || rect.height == 0)
        {
            return EventOutcome::Ignored;
        }
        self.child.event(event, layout, cx)
    }
}
fn render_registry(
    linked: &LinkedRegistry,
    states: &InstanceState,
    active: Option<&str>,
    interactions: Option<&InstanceState>,
) -> Result<bmux_presenter::CustomRegistry<'static>, HostError> {
    let mut grouped: BTreeMap<String, BTreeMap<String, LinkedState>> = BTreeMap::new();
    for (key, state) in states {
        grouped
            .entry(key.component_type.clone())
            .or_default()
            .insert(key.id.clone(), Arc::clone(state));
    }
    let mut registry = bmux_presenter::CustomRegistry::new();
    for (name, states) in grouped {
        let factory = Arc::clone(linked.factory(&name).ok_or(HostError::Invalid)?);
        registry
            .register(
                name.clone(),
                BoundFactory {
                    factory,
                    interactions: interactions
                        .into_iter()
                        .flat_map(|states| states.iter())
                        .filter(|(key, _)| key.component_type == name)
                        .map(|(key, state)| (key.id.clone(), Arc::clone(state)))
                        .collect(),
                    states,
                    active: active.map(str::to_owned),
                },
            )
            .map_err(|_| HostError::Invalid)?;
    }
    Ok(registry)
}
impl Owner for LinkedOwner {
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
    fn commit_reload(&mut self, candidate: &mut dyn Owner) -> Result<(), HostError> {
        if self.demos.is_empty() {
            return Ok(());
        }
        // Reject unknown replacement ownership without touching live resources.
        let candidate = candidate
            .as_any_mut()
            .and_then(|owner| owner.downcast_mut::<Self>())
            .ok_or(HostError::Invalid)?;
        if self
            .demos
            .keys()
            .any(|instance| !candidate.demos.contains_key(instance))
        {
            return Err(HostError::Invalid);
        }
        let mut deferred = false;
        for (instance, demos) in &mut self.demos {
            if demos.admit_reload(&candidate.demos[instance]).is_err() {
                deferred = true;
            }
        }
        if deferred {
            return Err(HostError::EffectUncertain);
        }
        for (instance, demos) in &mut self.demos {
            demos.commit_reload(
                candidate
                    .demos
                    .get_mut(instance)
                    .expect("validated instance"),
            );
        }
        Ok(())
    }
    fn shutdown(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), HostError>> + Send + '_>>
    {
        Box::pin(async move {
            for demos in self.demos.values_mut() {
                demos
                    .shutdown()
                    .await
                    .map_err(|_| HostError::EffectUncertain)?;
            }
            Ok(())
        })
    }
    fn disconnect(&mut self, participant: &str) {
        self.interactions
            .retain(|(owner, _), _| owner != participant);
        self.layouts.retain(|(owner, _, _), _| owner != participant);
    }
    fn advance(&mut self, context: OwnerContext<'_>) -> Result<bool, HostError> {
        if !self.demos.contains_key(context.instance)
            && let Some(environment) = &self.demo_environment
        {
            let demos = crate::playbook_live::LiveDemos::prepare(&self.deck, environment)
                .map_err(|_| HostError::Invalid)?;
            self.demos.insert(context.instance.into(), demos);
        }
        Ok(self
            .demos
            .get_mut(context.instance)
            .is_some_and(crate::playbook_live::LiveDemos::update))
    }
    fn navigation(
        &mut self,
        previous_slide: Option<&str>,
        context: OwnerContext<'_>,
    ) -> Result<(), HostError> {
        if previous_slide != context.state.slide_id.as_ref().map(|id| id.as_str())
            && let (Some(slide), Some(demos)) =
                (previous_slide, self.demos.get_mut(context.instance))
        {
            demos.leave(slide).map_err(|_| HostError::Invalid)?;
        }
        Ok(())
    }
    fn prepare_reload(
        &self,
        deck: &Deck,
        assets: &[bmux_presenter_runtime::Asset],
    ) -> Result<Box<dyn Owner>, HostError> {
        let mut rich = crate::rich::RichAssets::prepare(assets).map_err(|_| HostError::Invalid)?;
        rich.code = std::sync::Arc::new(crate::rich::highlight(deck));
        let mut candidate = LinkedOwner::prepare_candidate(self, deck.clone(), rich)?;
        // Detached snapshots only: no candidate tasks or live command handles.
        candidate.demo_environment = self.demo_environment.clone();
        for (instance, demos) in &self.demos {
            candidate.demos.insert(
                instance.clone(),
                demos.prepare_reload(deck).map_err(|_| HostError::Invalid)?,
            );
        }
        Ok(Box::new(candidate))
    }
    fn reconcile_reload(&mut self, context: OwnerContext<'_>) -> Result<Vec<MirrorFrame>, String> {
        self.scene(context)
    }
    fn view_scene(
        &mut self,
        context: OwnerContext<'_>,
        surfaces: &[MirrorFrame],
    ) -> Result<Vec<MirrorFrame>, HostError> {
        let mut projected = self.scenes(context)?;
        if projected.len() != surfaces.len() {
            return Err(HostError::Stale);
        }
        for surface in &mut projected {
            let base = surfaces
                .iter()
                .find(|base| base.surface_id == surface.surface_id)
                .ok_or(HostError::Stale)?;
            if base.slide_id != surface.slide_id
                || base.width != surface.width
                || base.height != surface.height
            {
                return Err(HostError::Invalid);
            }
            surface.revision = base.revision;
            if let Some(focused) = &mut surface.focused {
                focused.revision = base.revision;
            }
            surface.validate(16_384, 2 * 1024 * 1024)?;
        }
        Ok(projected)
    }
    fn scene(&mut self, context: OwnerContext<'_>) -> Result<Vec<MirrorFrame>, String> {
        self.scenes(context).map_err(|error| error.to_string())
    }
    fn demo_input(
        &mut self,
        context: OwnerContext<'_>,
        input: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        let geometry = self
            .layouts
            .get(&(
                context.participant.to_owned(),
                context.instance.to_owned(),
                input.surface_id.clone(),
            ))
            .ok_or(HostError::Stale)?;
        let slide = context
            .state
            .current_slide(&self.deck)
            .ok_or(HostError::Stale)?;
        if geometry
            .control_scene
            .as_ref()
            .map_or(geometry.interaction_revision, |scene| scene.frame.revision)
            != input.surface_revision
            || geometry.deck_revision != context.cursor.deck_revision
            || geometry.slide_id != slide.id.as_str()
            || geometry.reveal_step != context.state.reveal_step
        {
            return Err(HostError::Stale);
        }
        let block = custom_blocks(&slide.blocks, context.state.reveal_step)
            .into_iter()
            .find(|block| block.id.as_str() == input.surface_id)
            .ok_or(HostError::Stale)?;
        if let BlockKind::Custom { name, .. } = &block.kind
            && name == "playbook-demo"
        {
            let demos = self
                .demos
                .get_mut(context.instance)
                .ok_or(HostError::Unauthorized)?;
            let outcome = if let Some(scene) = &geometry.control_scene {
                scene
                    .interactions
                    .iter()
                    .find(|target| {
                        let area = target.area;
                        target.enabled
                            && input.x >= area.x
                            && input.y >= area.y
                            && u32::from(input.x) < u32::from(area.x) + u32::from(area.width)
                            && u32::from(input.y) < u32::from(area.y) + u32::from(area.height)
                            && (target.input == input.input
                                || target.input == "play-toggle"
                                    && matches!(input.input.as_str(), "run" | "pause" | "resume"))
                    })
                    .ok_or(HostError::Stale)?;
                let epoch = scene.identity["owner_epoch"]
                    .as_str()
                    .and_then(|value| uuid::Uuid::parse_str(value).ok())
                    .ok_or(HostError::Stale)?;
                let generation = scene.identity["generation"].as_u64();
                demos.input_committed(input.surface_id.as_str(), epoch, generation, &input.input)
            } else {
                if input.input == "click"
                    && !geometry.launcher.is_some_and(|area| {
                        area.contains(bmux_tui::geometry::Point::new(input.x, input.y))
                    })
                {
                    return Err(HostError::Stale);
                }
                demos.input(input.surface_id.as_str(), &input.input, false)
            }
            .map_err(|_| HostError::Unauthorized)?;
            return Ok(OwnerUpdate {
                outcome,
                surfaces: self.scenes(context)?,
            });
        }
        let states = self
            .instances
            .get(context.instance)
            .ok_or(HostError::Stale)?;
        if self.interactions.len() >= 256
            && !self
                .interactions
                .contains_key(&(context.participant.to_owned(), context.instance.to_owned()))
        {
            return Err(HostError::Limit);
        }
        let interactions = self
            .interactions
            .entry((context.participant.to_owned(), context.instance.to_owned()))
            .or_default();
        for key in states.keys() {
            interactions
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(serde_json::json!({}))));
        }
        let registry = render_registry(
            &self.linked,
            states,
            Some(&input.surface_id),
            Some(interactions),
        )?;
        let theme = bmux_presenter::PresentationTheme::for_deck(&self.deck);
        let component = bmux_presenter::SlideComponent::new(
            slide,
            context.state.reveal_step,
            &theme,
            &registry,
        )
        .rich_content(&self.rich)
        .focused_block(&block.id);
        let event = crate::managed_input::decode(&input.input, input.x, input.y)
            .ok_or(HostError::Invalid)?;
        let outcome = component.event(
            &event,
            &geometry.layout,
            &mut EventCx::with_clip(
                &geometry.layout,
                bmux_tui::geometry::Rect::new(
                    0,
                    0,
                    u16::try_from(geometry.layout.size.width).map_err(|_| HostError::Limit)?,
                    u16::try_from(geometry.layout.size.height).map_err(|_| HostError::Limit)?,
                ),
            ),
        );
        self.layouts.remove(&(
            context.participant.to_owned(),
            context.instance.to_owned(),
            input.surface_id.clone(),
        ));
        let outcome = crate::managed_input::focus_ack(
            &input.surface_id,
            input.surface_revision,
            &input.input,
            outcome != EventOutcome::Ignored,
        )
        .and_then(|ack| crate::managed_input::encode_ack(&ack))
        .unwrap_or_else(|| {
            if outcome == EventOutcome::Ignored {
                "input outside active control".into()
            } else {
                "component updated".into()
            }
        });
        Ok(OwnerUpdate {
            outcome,
            surfaces: self.scenes(context)?,
        })
    }
    fn effect(
        &mut self,
        context: OwnerContext<'_>,
        name: &str,
        input: &str,
    ) -> Result<OwnerUpdate, HostError> {
        let id = name
            .strip_prefix("playbook-demo:")
            .ok_or(HostError::Unauthorized)?;
        let slide = context
            .state
            .current_slide(&self.deck)
            .ok_or(HostError::Stale)?;
        if !custom_blocks(&slide.blocks, context.state.reveal_step).iter().any(|block| block.id.as_str() == id && matches!(&block.kind, BlockKind::Custom { name, .. } if name == "playbook-demo")) {
            return Err(HostError::Stale);
        }
        let demos = self
            .demos
            .get_mut(context.instance)
            .ok_or(HostError::Unauthorized)?;
        let outcome = if let Some(scene) = self
            .layouts
            .get(&(
                context.participant.to_owned(),
                context.instance.to_owned(),
                id.to_owned(),
            ))
            .and_then(|geometry| geometry.control_scene.as_ref())
        {
            let epoch = scene.identity["owner_epoch"]
                .as_str()
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or(HostError::Stale)?;
            demos.input_committed(id, epoch, scene.identity["generation"].as_u64(), input)
        } else if matches!(input, "open" | "o" | "click" | "close" | "escape") {
            demos.input(id, input, true)
        } else {
            return Err(HostError::Stale);
        }
        .map_err(|_| HostError::Invalid)?;
        Ok(OwnerUpdate {
            outcome,
            surfaces: self.scenes(context)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn managed_open_demo_publishes_optional_terminal_with_wire_budgets() {
        use crate::playbook_live::reload_tests::{Fixture, deck};
        let fixture = Fixture::new();
        let running = fixture.running();
        let mut owner =
            LinkedOwner::new(deck("s", "old"), &[], crate::demo::linked_registry()).unwrap();
        let state = bmux_presenter::PresentationState::new(&owner.deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "audience-run".into(),
            deck_revision: 1,
            revision: 1,
        };
        owner.demos.insert("shared".into(), running.demos);
        let context = || OwnerContext {
            participant: "p",
            instance: "shared",
            state: &state,
            cursor: &cursor,
        };
        let open = owner.scenes(context()).unwrap();
        let frame = &open[0];
        let playbook = frame.playbook.as_ref().unwrap();
        let source = &owner.demos["shared"].entries["demo"]
            .snapshot
            .surface
            .as_ref()
            .unwrap()
            .frame;
        assert_eq!(playbook.terminal.buffer(), source.buffer());
        assert_eq!(playbook.terminal.cursor(), source.cursor());
        assert_eq!(playbook.terminal.generation(), "audience-run");
        assert_eq!(playbook.terminal.revision(), frame.revision);
        frame.validate(16_384, 2 * 1024 * 1024).unwrap();
        let total_cells = frame.cells.len() + playbook.terminal.buffer().cells().len();
        assert!(frame.validate(total_cells - 1, 2 * 1024 * 1024).is_err());
        let wire = serde_json::to_vec(frame).unwrap();
        assert!(frame.validate(16_384, wire.len() - 1).is_err());
        frame.validate(16_384, wire.len()).unwrap();
        let restored: MirrorFrame = serde_json::from_slice(&wire).unwrap();
        assert_eq!(restored.playbook.as_ref().unwrap().title, playbook.title);
        let mut invalid = restored;
        invalid.playbook.as_mut().unwrap().progress = 10001;
        assert!(invalid.validate(16_384, 2 * 1024 * 1024).is_err());
        owner
            .demos
            .get_mut("shared")
            .unwrap()
            .entries
            .get_mut("demo")
            .unwrap()
            .open = false;
        let closed = owner.scenes(context()).unwrap();
        assert!(closed[0].playbook.is_none());
        let json = serde_json::to_string(&closed[0]).unwrap();
        assert!(!json.contains("\"playbook\""));
        let restored: MirrorFrame = serde_json::from_str(&json).unwrap();
        assert!(restored.playbook.is_none());
    }

    #[tokio::test]
    async fn managed_reload_handoff_preserves_active_epoch_surface_and_approval() {
        use crate::playbook_live::reload_tests::{Fixture, deck, empty};
        let fixture = Fixture::new();
        let running = fixture.running();
        let epoch = running.demos.entries["demo"].controller.owner_epoch();
        let surface = running.demos.entries["demo"]
            .snapshot
            .surface
            .clone()
            .unwrap();
        let mut owner =
            LinkedOwner::new(deck("s", "old"), &[], crate::demo::linked_registry()).unwrap();
        owner.demo_environment = Some(fixture.environment.clone());
        owner.demos.insert("shared".into(), running.demos);
        let mut candidate = owner
            .prepare_reload(&deck("moved", "renamed"), &[])
            .unwrap();
        assert!(!*running.stopped.borrow());
        assert_eq!(*running.cancelled.borrow(), 0);
        owner.commit_reload(candidate.as_mut()).unwrap();
        let candidate = candidate
            .as_any_mut()
            .unwrap()
            .downcast_mut::<LinkedOwner>()
            .unwrap();
        let entry = &candidate.demos["shared"].entries["demo"];
        assert_eq!(entry.controller.owner_epoch(), epoch);
        assert_eq!(entry.snapshot.attempt.as_ref().unwrap().generation, 41);
        assert!(Arc::ptr_eq(
            entry.snapshot.surface.as_ref().unwrap(),
            &surface
        ));
        assert_eq!(entry.slide, "moved");
        assert!(!*running.stopped.borrow());
        std::fs::write(
            fixture.root.join("steps.dsl"),
            "new-session\nsend-keys keys=changed\n",
        )
        .unwrap();
        assert!(
            candidate
                .prepare_reload(&deck("moved", "renamed"), &[])
                .is_err()
        );
        assert_eq!(
            candidate.demos["shared"].entries["demo"]
                .controller
                .owner_epoch(),
            epoch
        );
        assert_eq!(*running.cancelled.borrow(), 0);
        // Removing a reference transfers ownership without cancelling execution.
        let mut removed = candidate.prepare_reload(&empty(), &[]).unwrap();
        candidate.commit_reload(removed.as_mut()).unwrap();
        assert_eq!(*running.cancelled.borrow(), 0);
        assert!(!*running.stopped.borrow());
    }
    fn controls() -> Deck {
        bmux_presenter::DeckBuilder::new("linked")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom("count", "counter", serde_json::json!({}))
                    .custom("switch", "toggle", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap()
    }
    fn context<'a>(
        instance: &'a str,
        state: &'a bmux_presenter::PresentationState,
        cursor: &'a bmux_presenter_runtime::Cursor,
    ) -> OwnerContext<'a> {
        OwnerContext {
            participant: instance,
            instance,
            state,
            cursor,
        }
    }
    fn value(owner: &LinkedOwner, instance: &str, id: &str) -> serde_json::Value {
        owner.instances[instance]
            .iter()
            .find(|(key, _)| key.id == id)
            .unwrap()
            .1
            .lock()
            .unwrap()
            .clone()
    }
    #[test]
    fn owner_focus_projection_is_viewer_selectable_without_mutating_shared_state() {
        let deck = bmux_presenter::DeckBuilder::new("focus")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom(
                        "edit",
                        "tui-gallery",
                        serde_json::json!({"kind":"text-input-box"}),
                    )
                    .custom("static", "tui-gallery", serde_json::json!({"kind":"badge"}))
                    .build(),
            )
            .build()
            .unwrap();
        let state = bmux_presenter::PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner = LinkedOwner::new(deck, &[], crate::demo::linked_registry()).unwrap();
        let frames = owner.scenes(context("shared", &state, &cursor)).unwrap();
        let previous = value(&owner, "shared", "edit");
        let edit = frames
            .iter()
            .find(|frame| frame.surface_id == "edit")
            .unwrap();
        let focused = edit.focused.as_ref().unwrap();
        assert!(edit.focusable);
        assert!(edit.cursor.is_none());
        assert!(focused.cursor.is_some());
        assert_eq!(edit.width, focused.width);
        assert_eq!(edit.height, focused.height);
        assert_eq!(edit.revision, focused.revision);
        assert_eq!(edit.interaction_revision, focused.interaction_revision);
        assert!(focused.focused.is_none());
        assert!(focused.private_scene.is_none());
        assert!(focused.playbook.is_none());
        let static_frame = frames
            .iter()
            .find(|frame| frame.surface_id == "static")
            .unwrap();
        assert!(!static_frame.focusable);
        assert!(static_frame.focused.is_none());
        owner.scenes(context("shared", &state, &cursor)).unwrap();
        assert_eq!(value(&owner, "shared", "edit"), previous);
    }
    #[test]
    fn transient_owner_interaction_is_independent_for_participants_sharing_values() {
        let deck = bmux_presenter::DeckBuilder::new("participants")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom(
                        "menu",
                        "tui-gallery",
                        serde_json::json!({"kind":"select-dropdown"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let state = bmux_presenter::PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner = LinkedOwner::new(deck, &[], crate::demo::linked_registry()).unwrap();
        let context = |participant| OwnerContext {
            participant,
            instance: "shared",
            state: &state,
            cursor: &cursor,
        };
        let frames = owner.scenes(context("one")).unwrap();
        let canonical = value(&owner, "shared", "menu");
        owner
            .demo_input(
                context("one"),
                &SurfaceInput {
                    surface_id: "menu".into(),
                    surface_revision: frames[0].interaction_revision.unwrap(),
                    x: 0,
                    y: 0,
                    input: crate::managed_input::encode(&Event::Key(
                        bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Enter),
                    ))
                    .unwrap(),
                },
            )
            .unwrap();
        let one = owner.scenes(context("one")).unwrap();
        let two = owner.scenes(context("two")).unwrap();
        assert_eq!(value(&owner, "shared", "menu"), canonical);
        assert_eq!(
            serde_json::to_value(&one[0].cells).unwrap(),
            serde_json::to_value(&two[0].cells).unwrap()
        );
        assert_ne!(
            serde_json::to_value(&one[0].focused.as_ref().unwrap().cells).unwrap(),
            serde_json::to_value(&two[0].focused.as_ref().unwrap().cells).unwrap()
        );
        let one_interaction = owner.interactions[&("one".into(), "shared".into())]
            .values()
            .next()
            .unwrap();
        let two_interaction = owner.interactions[&("two".into(), "shared".into())]
            .values()
            .next()
            .unwrap();
        assert!(!Arc::ptr_eq(one_interaction, two_interaction));
        let one_value = one_interaction.lock().unwrap().clone();
        let two_value = two_interaction.lock().unwrap().clone();
        assert_eq!(one_value["menu"]["native"]["open"], serde_json::json!(true));
        assert!(two_value.get("menu").is_none());
        owner.disconnect("one");
        assert!(
            !owner
                .interactions
                .contains_key(&("one".into(), "shared".into()))
        );
        assert!(
            owner
                .interactions
                .contains_key(&("two".into(), "shared".into()))
        );
    }
    #[test]
    fn managed_codec_routes_full_events_only_after_revision_validation() {
        struct Recorder;
        struct RecordingControl(LinkedState);
        impl LinkedComponentFactory for Recorder {
            fn state_version(&self) -> u64 {
                1
            }
            fn initial_state(&self, _block: &Block) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!([]))
            }
            fn build<'a>(
                &'a self,
                _block: &'a Block,
                state: LinkedState,
            ) -> Result<Element<'a>, String> {
                Ok(Element::new(RecordingControl(state)))
            }
        }
        impl Component for RecordingControl {
            fn layout(&self, c: Constraints, cx: &mut LayoutCx) -> LayoutNode {
                TextBlock::new("record").layout(c, cx)
            }
            fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}
            fn event(
                &self,
                event: &Event,
                _layout: &LayoutNode,
                _cx: &mut EventCx<'_>,
            ) -> EventOutcome {
                self.0
                    .lock()
                    .unwrap()
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!(format!("{event:?}")));
                EventOutcome::Redraw
            }
        }
        let deck = bmux_presenter::DeckBuilder::new("codec")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom("record", "record", serde_json::Value::Null)
                    .build(),
            )
            .build()
            .unwrap();
        let state = bmux_presenter::PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut linked = LinkedRegistry::default();
        linked.register("record", Recorder).unwrap();
        let mut owner = LinkedOwner::new(deck, &[], linked).unwrap();
        for event in [
            Event::Key(bmux_keyboard::KeyStroke::with_modifiers(
                bmux_keyboard::KeyCode::Delete,
                bmux_keyboard::Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
            )),
            Event::Paste("é bounded".into()),
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::ScrollDown,
                bmux_tui::geometry::Point::new(0, 0),
            )),
        ] {
            let frames = owner.scenes(context("shared", &state, &cursor)).unwrap();
            let input = SurfaceInput {
                surface_id: "record".into(),
                surface_revision: frames[0].interaction_revision.unwrap_or(frames[0].revision),
                x: 0,
                y: 0,
                input: crate::managed_input::encode(&event).unwrap(),
            };
            let previous = value(&owner, "shared", "record");
            let mut stale = input.clone();
            stale.surface_revision += 1;
            assert!(matches!(
                owner.demo_input(context("shared", &state, &cursor), &stale),
                Err(HostError::Stale)
            ));
            assert_eq!(value(&owner, "shared", "record"), previous);
            owner
                .demo_input(context("shared", &state, &cursor), &input)
                .unwrap();
            let recorded = value(&owner, "shared", "record");
            assert_eq!(
                recorded.as_array().unwrap().last().unwrap(),
                &serde_json::json!(format!("{event:?}"))
            );
        }
        owner.scenes(context("private", &state, &cursor)).unwrap();
        assert_eq!(value(&owner, "private", "record"), serde_json::json!([]));
        let frames = owner.scenes(context("shared", &state, &cursor)).unwrap();
        let previous = value(&owner, "shared", "record");
        assert!(matches!(
            owner.demo_input(
                context("shared", &state, &cursor),
                &SurfaceInput {
                    surface_id: "record".into(),
                    surface_revision: frames[0].interaction_revision.unwrap_or(frames[0].revision),
                    x: 0,
                    y: 0,
                    input: "presenter-input-v1:{}".into(),
                }
            ),
            Err(HostError::Invalid)
        ));
        assert_eq!(value(&owner, "shared", "record"), previous);
    }
    #[test]
    fn public_embedding_constructor_hosts_both_linked_types() {
        let deck = controls();
        let owner = LinkedOwner::new(deck.clone(), &[], crate::demo::linked_registry()).unwrap();
        let mut host = bmux_presenter_runtime::Host::new(
            deck,
            vec![],
            bmux_presenter_runtime::HostConfig {
                public_audience: true,
                ..Default::default()
            },
        )
        .unwrap();
        host.set_owner(Box::new(owner)).unwrap();
        let participant = host
            .authenticate(
                bmux_presenter_runtime::PROTOCOL_VERSION,
                None,
                bmux_presenter_runtime::InteractionMode::ReceiveOnly,
            )
            .unwrap();
        let snapshot = host.snapshot(&participant).unwrap();
        assert_eq!(
            snapshot
                .surfaces
                .iter()
                .map(|surface| surface.surface_id.as_str())
                .collect::<Vec<_>>(),
            vec!["count", "switch"]
        );
    }
    #[test]
    fn named_surfaces_target_one_control_and_share_only_the_same_instance() {
        let deck = controls();
        let state = bmux_presenter::PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner = LinkedOwner::with_examples(deck, crate::rich::RichAssets::default());
        let frames = owner.scenes(context("shared", &state, &cursor)).unwrap();
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.surface_id.as_str())
                .collect::<Vec<_>>(),
            vec!["count", "switch"]
        );
        let input = SurfaceInput {
            surface_id: "switch".into(),
            surface_revision: frames[1].revision,
            x: 0,
            y: 0,
            input: " ".into(),
        };
        owner
            .demo_input(context("shared", &state, &cursor), &input)
            .unwrap();
        assert_eq!(value(&owner, "shared", "switch"), serde_json::json!(true));
        assert_eq!(value(&owner, "shared", "count"), serde_json::json!(0));
        assert!(matches!(
            owner.demo_input(context("shared", &state, &cursor), &input),
            Err(HostError::Stale)
        ));
        owner.scenes(context("shared", &state, &cursor)).unwrap();
        owner.scenes(context("private", &state, &cursor)).unwrap();
        assert_eq!(value(&owner, "shared", "switch"), serde_json::json!(true));
        assert_eq!(value(&owner, "private", "switch"), serde_json::json!(false));
    }
    #[test]
    fn reload_retains_compatible_values_resets_changed_type_and_is_detached() {
        let deck = controls();
        let state = bmux_presenter::PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner =
            LinkedOwner::with_examples(deck.clone(), crate::rich::RichAssets::default());
        for instance in ["shared", "private"] {
            owner.scenes(context(instance, &state, &cursor)).unwrap();
        }
        for states in owner.instances.values() {
            for (key, state) in states {
                if key.id == "count" {
                    *state.lock().unwrap() = serde_json::json!(7);
                }
            }
        }
        let mut candidate = owner
            .prepare_candidate(deck.clone(), crate::rich::RichAssets::default())
            .unwrap();
        candidate
            .scenes(context("shared", &state, &cursor))
            .unwrap();
        assert_eq!(value(&candidate, "shared", "count"), serde_json::json!(7));
        assert_eq!(value(&candidate, "private", "count"), serde_json::json!(7));
        let key = candidate.instances["shared"]
            .keys()
            .find(|key| key.id == "count")
            .unwrap()
            .clone();
        *candidate.instances["shared"][&key].lock().unwrap() = serde_json::json!(99);
        assert_eq!(value(&owner, "shared", "count"), serde_json::json!(7));
        let mut changed = deck.clone();
        changed.slides[0].blocks[0].kind = BlockKind::Custom {
            name: "toggle".into(),
            config: serde_json::json!({}),
        };
        let changed_state = bmux_presenter::PresentationState::new(&changed);
        let mut reset = owner
            .prepare_candidate(changed, crate::rich::RichAssets::default())
            .unwrap();
        reset
            .scenes(context("shared", &changed_state, &cursor))
            .unwrap();
        assert_eq!(value(&reset, "shared", "count"), serde_json::json!(false));
        let invalid = bmux_presenter_runtime::Asset {
            id: "bad".into(),
            media_type: "image/png".into(),
            bytes: vec![0],
        };
        assert!(Owner::prepare_reload(&owner, &deck, &[invalid]).is_err());
        assert_eq!(value(&owner, "shared", "count"), serde_json::json!(7));
    }
    #[test]
    fn version_change_resets_state_and_stale_reveal_geometry_is_rejected() {
        struct VersionTwo;
        impl LinkedComponentFactory for VersionTwo {
            fn state_version(&self) -> u64 {
                2
            }
            fn initial_state(&self, _block: &Block) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!(42))
            }
            fn build<'a>(
                &'a self,
                _block: &'a Block,
                _state: LinkedState,
            ) -> Result<Element<'a>, String> {
                Ok(Element::new(bmux_tui::prelude::TextBlock::new(
                    "version two",
                )))
            }
        }
        let deck = controls();
        let mut state = bmux_presenter::PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner =
            LinkedOwner::with_examples(deck.clone(), crate::rich::RichAssets::default());
        let frames = owner.scenes(context("shared", &state, &cursor)).unwrap();
        state.reveal_step = 1;
        assert!(matches!(
            owner.demo_input(
                context("shared", &state, &cursor),
                &SurfaceInput {
                    surface_id: "count".into(),
                    surface_revision: frames[0].revision,
                    x: 0,
                    y: 0,
                    input: "+".into()
                }
            ),
            Err(HostError::Stale)
        ));
        let mut upgraded = LinkedRegistry::default();
        upgraded.register("counter", VersionTwo).unwrap();
        owner.linked = upgraded;
        let mut candidate = owner
            .prepare_candidate(deck, crate::rich::RichAssets::default())
            .unwrap();
        candidate
            .scenes(context("shared", &state, &cursor))
            .unwrap();
        assert_eq!(value(&candidate, "shared", "count"), serde_json::json!(42));
        assert_eq!(value(&owner, "shared", "count"), serde_json::json!(0));
    }
    #[test]
    fn prepared_owner_reload_is_atomic_and_reconciles_scene_identity() {
        let original = bmux_presenter::DeckBuilder::new("reload")
            .slide(
                bmux_presenter::SlideBuilder::new("old")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        let mut host = bmux_presenter_runtime::Host::new(
            original.clone(),
            vec![],
            bmux_presenter_runtime::HostConfig {
                public_audience: true,
                ..Default::default()
            },
        )
        .unwrap();
        host.set_owner(Box::new(LinkedOwner::with_examples(
            original,
            crate::rich::RichAssets::default(),
        )))
        .unwrap();
        let viewer = host
            .authenticate(
                bmux_presenter_runtime::PROTOCOL_VERSION,
                None,
                bmux_presenter_runtime::InteractionMode::ReceiveOnly,
            )
            .unwrap();
        let before = host.snapshot(&viewer).unwrap();
        let replacement = bmux_presenter::DeckBuilder::new("reload")
            .slide(
                bmux_presenter::SlideBuilder::new("new")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        host.reload(replacement, vec![]).unwrap();
        let after = host.snapshot(&viewer).unwrap();
        assert_eq!(after.cursor.run_id, before.cursor.run_id);
        assert!(after.cursor.deck_revision > before.cursor.deck_revision);
        assert_eq!(after.surfaces[0].slide_id, "new");
        assert_eq!(after.state.slide_id, Some("new".into()));
    }
    #[test]
    fn host_followers_share_committed_selection_but_not_dropdown_interaction() {
        use bmux_presenter_runtime::{
            Action, Credential, Host, HostConfig, InteractionMode, PROTOCOL_VERSION, Scopes,
            Snapshot, Submission,
        };
        fn join(host: &mut Host, mode: InteractionMode) -> String {
            let credential = Credential::generate(
                Scopes {
                    demo_input: true,
                    ..Default::default()
                },
                std::time::Duration::from_secs(60),
            );
            let secret = credential.secret.clone();
            host.add_credential(credential).unwrap();
            host.authenticate(PROTOCOL_VERSION, Some(&secret), mode)
                .unwrap()
        }
        fn send(host: &mut Host, participant: &str, key: bmux_keyboard::KeyCode) {
            let snapshot = host.snapshot(participant).unwrap();
            let surface = snapshot
                .surfaces
                .iter()
                .find(|surface| surface.surface_id == "menu")
                .unwrap();
            host.submit(
                participant,
                Submission {
                    action_id: host.next_action_id(participant).unwrap().unwrap(),
                    expected: snapshot.cursor,
                    action: Action::DemoInput(SurfaceInput {
                        surface_id: surface.surface_id.clone(),
                        surface_revision: surface.interaction_revision.unwrap_or(surface.revision),
                        x: 0,
                        y: 0,
                        input: crate::managed_input::encode(&Event::Key(
                            bmux_keyboard::KeyStroke::simple(key),
                        ))
                        .unwrap(),
                    }),
                },
            )
            .unwrap();
        }
        fn menu(snapshot: &Snapshot) -> &MirrorFrame {
            snapshot
                .surfaces
                .iter()
                .find(|surface| surface.surface_id == "menu")
                .unwrap()
        }
        fn text(surface: &MirrorFrame) -> String {
            surface
                .cells
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect()
        }
        fn cells(surface: &MirrorFrame) -> serde_json::Value {
            serde_json::to_value(&surface.cells).unwrap()
        }
        fn no_private_metadata(surface: &MirrorFrame) -> bool {
            surface.focused.is_none() && surface.cursor.is_none() && surface.private_scene.is_none()
        }
        let deck = bmux_presenter::DeckBuilder::new("followers")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom(
                        "menu",
                        "tui-gallery",
                        serde_json::json!({"kind":"select-dropdown"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let mut host = Host::new(deck.clone(), vec![], HostConfig::default()).unwrap();
        host.set_owner(Box::new(
            LinkedOwner::new(deck, &[], crate::demo::linked_registry()).unwrap(),
        ))
        .unwrap();
        // Separate credentials are deliberate: action ledgers are credential-scoped,
        // while transient interaction must be participant-scoped, not shared-instance.
        let a = join(&mut host, InteractionMode::Shared);
        let b = join(&mut host, InteractionMode::Shared);
        let readonly = join(&mut host, InteractionMode::ReceiveOnly);
        let initial_a = host.snapshot(&a).unwrap();
        let initial_b = host.snapshot(&b).unwrap();
        assert!(menu(&initial_a).focused.is_some());
        assert_eq!(
            cells(menu(&initial_a).focused.as_ref().unwrap()),
            cells(menu(&initial_b).focused.as_ref().unwrap())
        );
        assert!(text(menu(&initial_b)).contains("Ready"));
        send(&mut host, &a, bmux_keyboard::KeyCode::Enter);
        let opened_a = host.snapshot(&a).unwrap();
        let bmux_presenter_runtime::Poll::Snapshot {
            snapshot: unchanged_b,
            ..
        } = host.poll(&b, Some(&initial_b.cursor)).unwrap()
        else {
            panic!("follower must observe new shared revision through poll");
        };
        let unchanged_b = *unchanged_b;
        assert_eq!(cells(menu(&opened_a)), cells(menu(&unchanged_b)));
        assert_eq!(
            cells(menu(&initial_b).focused.as_ref().unwrap()),
            cells(menu(&unchanged_b).focused.as_ref().unwrap())
        );
        assert_ne!(
            cells(menu(&opened_a).focused.as_ref().unwrap()),
            cells(menu(&unchanged_b).focused.as_ref().unwrap())
        );
        send(&mut host, &a, bmux_keyboard::KeyCode::Down);
        let moved_a = host.snapshot(&a).unwrap();
        let still_b = host.snapshot(&b).unwrap();
        assert_ne!(
            cells(menu(&moved_a).focused.as_ref().unwrap()),
            cells(menu(&opened_a).focused.as_ref().unwrap())
        );
        assert_eq!(
            cells(menu(&still_b).focused.as_ref().unwrap()),
            cells(menu(&initial_b).focused.as_ref().unwrap())
        );
        assert!(
            text(menu(&still_b)).contains("Ready"),
            "active option is not committed selection"
        );
        let readonly_snapshot = host.snapshot(&readonly).unwrap();
        assert!(readonly_snapshot.surfaces.iter().all(no_private_metadata));
        let readonly_surface = menu(&readonly_snapshot);
        assert!(matches!(
            host.submit(
                &readonly,
                Submission {
                    action_id: host.next_action_id(&readonly).unwrap().unwrap(),
                    expected: readonly_snapshot.cursor.clone(),
                    action: Action::DemoInput(SurfaceInput {
                        surface_id: "menu".into(),
                        surface_revision: readonly_surface
                            .interaction_revision
                            .unwrap_or(readonly_surface.revision),
                        x: 0,
                        y: 0,
                        input: "click".into()
                    }),
                }
            ),
            Err(HostError::Unauthorized)
        ));
        send(&mut host, &a, bmux_keyboard::KeyCode::Enter);
        let committed_a = host.snapshot(&a).unwrap();
        let committed_b = host.snapshot(&b).unwrap();
        assert!(text(menu(&committed_a)).contains("Running"));
        assert!(text(menu(&committed_b)).contains("Running"));
        assert_eq!(cells(menu(&committed_a)), cells(menu(&committed_b)));
        assert!(
            host.snapshot(&readonly)
                .unwrap()
                .surfaces
                .iter()
                .all(no_private_metadata)
        );
        // B can open and move independently; A's committed closed projection stays put.
        let closed_a = cells(menu(&committed_a).focused.as_ref().unwrap());
        send(&mut host, &b, bmux_keyboard::KeyCode::Enter);
        send(&mut host, &b, bmux_keyboard::KeyCode::Down);
        assert_eq!(
            cells(menu(&host.snapshot(&a).unwrap()).focused.as_ref().unwrap()),
            closed_a
        );
    }
    #[test]
    fn host_followers_with_one_credential_keep_active_list_choices_personal() {
        use bmux_presenter_runtime::{
            Action, Credential, Host, HostConfig, InteractionMode, PROTOCOL_VERSION, Scopes,
            Submission,
        };
        fn send(host: &mut Host, participant: &str, key: bmux_keyboard::KeyCode) {
            let snapshot = host.snapshot(participant).unwrap();
            let surface = &snapshot.surfaces[0];
            host.submit(
                participant,
                Submission {
                    action_id: host.next_action_id(participant).unwrap().unwrap(),
                    expected: snapshot.cursor,
                    action: Action::DemoInput(SurfaceInput {
                        surface_id: surface.surface_id.clone(),
                        surface_revision: surface.interaction_revision.unwrap_or(surface.revision),
                        x: 0,
                        y: 0,
                        input: crate::managed_input::encode(&Event::Key(
                            bmux_keyboard::KeyStroke::simple(key),
                        ))
                        .unwrap(),
                    }),
                },
            )
            .unwrap();
        }
        fn cells(surface: &MirrorFrame) -> serde_json::Value {
            serde_json::to_value(&surface.cells).unwrap()
        }
        let deck = bmux_presenter::DeckBuilder::new("followers-list")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom(
                        "list",
                        "tui-gallery",
                        serde_json::json!({"kind":"selectable-list"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let mut host = Host::new(deck.clone(), vec![], HostConfig::default()).unwrap();
        host.set_owner(Box::new(
            LinkedOwner::new(deck, &[], crate::demo::linked_registry()).unwrap(),
        ))
        .unwrap();
        let credential = Credential::generate(
            Scopes {
                demo_input: true,
                ..Default::default()
            },
            std::time::Duration::from_secs(60),
        );
        let secret = credential.secret.clone();
        host.add_credential(credential).unwrap();
        let a = host
            .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)
            .unwrap();
        let b = host
            .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)
            .unwrap();
        let initial_a = host.snapshot(&a).unwrap().surfaces.remove(0);
        let initial_b = host.snapshot(&b).unwrap().surfaces.remove(0);
        // Both participants establish a transient active choice before A commits.
        send(&mut host, &a, bmux_keyboard::KeyCode::Down);
        send(&mut host, &b, bmux_keyboard::KeyCode::Down);
        send(&mut host, &b, bmux_keyboard::KeyCode::Down);
        let moved_a = host.snapshot(&a).unwrap().surfaces.remove(0);
        let moved_b = host.snapshot(&b).unwrap().surfaces.remove(0);
        assert_eq!(cells(&moved_a), cells(&initial_a));
        assert_eq!(cells(&moved_b), cells(&initial_b));
        assert_ne!(
            cells(moved_a.focused.as_ref().unwrap()),
            cells(moved_b.focused.as_ref().unwrap())
        );
        // Transient snapshots must not overwrite a newer canonical selected value.
        send(&mut host, &a, bmux_keyboard::KeyCode::Enter);
        let committed_a = host.snapshot(&a).unwrap().surfaces.remove(0);
        let committed_b = host.snapshot(&b).unwrap().surfaces.remove(0);
        assert_ne!(cells(&initial_a), cells(&committed_a));
        assert_eq!(cells(&committed_a), cells(&committed_b));
        assert_ne!(
            cells(committed_a.focused.as_ref().unwrap()),
            cells(committed_b.focused.as_ref().unwrap()),
            "shared selection must not replace B's personal active item"
        );
        // B commits its own active item, not A's selected item restored from a stale snapshot.
        send(&mut host, &b, bmux_keyboard::KeyCode::Enter);
        let selected_a = host.snapshot(&a).unwrap().surfaces.remove(0);
        let selected_b = host.snapshot(&b).unwrap().surfaces.remove(0);
        assert_eq!(cells(&selected_a), cells(&selected_b));
        assert_ne!(cells(&selected_a), cells(&committed_a));
    }
    #[test]
    fn hosted_modes_publish_scenes_and_preserve_private_notes() {
        use bmux_presenter_runtime::{
            Action, Credential, Host, HostConfig, InteractionMode, PROTOCOL_VERSION, Scopes,
            Submission,
        };
        let mut deck = bmux_presenter::DeckBuilder::new("hosted")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        deck.slides[0].notes.text = "SECRET-NOTES".into();
        let mut host = Host::new(deck.clone(), vec![], HostConfig::default()).unwrap();
        host.set_owner(Box::new(LinkedOwner::with_examples(
            deck,
            crate::rich::RichAssets::default(),
        )))
        .unwrap();
        let presenter = Credential::generate(
            Scopes {
                notes: true,
                navigate: true,
                demo_input: true,
                effects: false,
            },
            std::time::Duration::from_secs(60),
        );
        let secret = presenter.secret.clone();
        host.add_credential(presenter).unwrap();
        let audience = Credential::generate(
            Scopes {
                notes: false,
                navigate: true,
                demo_input: true,
                effects: false,
            },
            std::time::Duration::from_secs(60),
        );
        let audience_secret = audience.secret.clone();
        host.add_credential(audience).unwrap();
        let readonly = host
            .authenticate(
                PROTOCOL_VERSION,
                Some(&audience_secret),
                InteractionMode::ReceiveOnly,
            )
            .unwrap();
        let snapshot = host.snapshot(&readonly).unwrap();
        assert_eq!(snapshot.surfaces.len(), 1);
        assert!(snapshot.notes.is_none());
        assert!(
            !serde_json::to_string(&snapshot.deck)
                .unwrap()
                .contains("SECRET-NOTES")
        );
        let submit = host
            .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::SubmitOnly)
            .unwrap();
        assert!(host.poll(&submit, None).is_err());
        let initial = host.snapshot(&submit).unwrap();
        assert_eq!(initial.notes.as_deref(), Some("SECRET-NOTES"));
        let frame = &initial.surfaces[0];
        let ack = host
            .submit(
                &submit,
                Submission {
                    action_id: host.next_action_id(&submit).unwrap().unwrap(),
                    expected: initial.cursor,
                    action: Action::DemoInput(SurfaceInput {
                        surface_id: frame.surface_id.clone(),
                        surface_revision: frame.interaction_revision.unwrap_or(frame.revision),
                        x: 0,
                        y: 0,
                        input: "+".into(),
                    }),
                },
            )
            .unwrap();
        assert_eq!(ack.action_id, 1);
        let shared = host
            .authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::Shared)
            .unwrap();
        let shared_frame = host.snapshot(&shared).unwrap().surfaces.remove(0);
        let text = shared_frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(text.contains("Counter: 1"));
        let independent = host
            .authenticate(
                PROTOCOL_VERSION,
                Some(&audience_secret),
                InteractionMode::Independent,
            )
            .unwrap();
        let independent_frame = host.snapshot(&independent).unwrap().surfaces.remove(0);
        let text = independent_frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(text.contains("Counter: 0"));
    }
    #[test]
    fn linked_owner_publishes_fixed_mirror_and_isolates_instances() {
        let deck = bmux_presenter::DeckBuilder::new("d")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .custom("demo", "counter", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        let state = bmux_presenter::PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner = LinkedOwner::with_examples(deck, crate::rich::RichAssets::default());
        let first = owner
            .scenes(OwnerContext {
                participant: "p",
                instance: "shared",
                state: &state,
                cursor: &cursor,
            })
            .unwrap();
        assert_eq!((first[0].width, first[0].height), (80, 38));
        let changed = owner
            .demo_input(
                OwnerContext {
                    participant: "p",
                    instance: "shared",
                    state: &state,
                    cursor: &cursor,
                },
                &SurfaceInput {
                    surface_id: "demo".into(),
                    surface_revision: first[0].revision,
                    x: 0,
                    y: 0,
                    input: "+".into(),
                },
            )
            .unwrap();
        assert!(changed.surfaces[0].revision > first[0].revision);
        assert_eq!(
            owner.instances["shared"]
                .values()
                .next()
                .unwrap()
                .lock()
                .unwrap()
                .as_i64(),
            Some(1)
        );
        owner
            .scenes(OwnerContext {
                participant: "p2",
                instance: "private",
                state: &state,
                cursor: &cursor,
            })
            .unwrap();
        assert_eq!(
            owner.instances["private"]
                .values()
                .next()
                .unwrap()
                .lock()
                .unwrap()
                .as_i64(),
            Some(0)
        );
    }
}
