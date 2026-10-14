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
}
/// Canonical fixed-canvas owner for trusted, application-registered components.
/// Shared and independent host instance identities select separate state maps.
/// Reload retains only matching block ID/component type/state-version keys.
pub struct LinkedOwner {
    deck: Deck,
    rich: crate::rich::RichAssets,
    linked: LinkedRegistry,
    instances: BTreeMap<String, InstanceState>,
    layouts: BTreeMap<(String, String), Geometry>,
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
        let registry = render_registry(&self.linked, states, None)?;
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
        let canvas_size = Size::new(80, 38);
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
            let operator_size = Size::new(80, 38);
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
                    512 * 1024,
                )?),
                interactions,
                identity: serde_json::json!({"id":id,"owner_epoch":demo.snapshot.owner_epoch.to_string(),
                    "generation":demo.snapshot.attempt.as_ref().map(|attempt| attempt.generation),
                    "paused":demo.snapshot.attempt.as_ref().map(|attempt| attempt.paused),
                    "phase":demo.snapshot.attempt.as_ref().map(|attempt| attempt.phase.as_str())}),
            });
            if let Some(scene) = &mut private_scene
                && let Some(previous) = self
                    .layouts
                    .get(&(context.instance.to_owned(), id.to_owned()))
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
                512 * 1024,
            )?;
            if overlay.is_some_and(|(active, _)| active == id) {
                surface.private_scene = private_scene.clone();
            }
            let interaction_revision = self
                .layouts
                .get(&(context.instance.to_owned(), id.clone()))
                .filter(|old| {
                    old.deck_revision == context.cursor.deck_revision
                        && old.slide_id == slide.id.as_str()
                        && old.reveal_step == context.state.reveal_step
                        && old.control_scene.is_none()
                        && private_scene.is_none()
                })
                .map_or(revision, |old| old.interaction_revision);
            surface.interaction_revision = Some(interaction_revision);
            surfaces.push(surface);
            geometries.insert(
                (context.instance.to_owned(), id),
                Geometry {
                    layout: layout.clone(),
                    slide_id: slide.id.as_str().into(),
                    reveal_step: context.state.reveal_step,
                    deck_revision: context.cursor.deck_revision,
                    interaction_revision,
                    control_scene: private_scene.clone(),
                },
            );
        }
        // Publish derived geometry only after every frame capture succeeds.
        self.layouts
            .retain(|(instance, _), _| instance != context.instance);
        self.layouts.extend(geometries);
        self.revisions.insert(context.instance.to_owned(), revision);
        Ok(surfaces)
    }
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
    active: Option<String>,
}
impl CustomComponentFactory for BoundFactory {
    fn build<'a>(&'a self, block: &'a Block) -> Result<Element<'a>, String> {
        let state = self
            .states
            .get(block.id.as_str())
            .ok_or("missing owner state")?;
        Ok(Element::new(TargetComponent {
            child: self.factory.build(block, Arc::clone(state))?,
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
                name,
                BoundFactory {
                    factory,
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
            .get(&(context.instance.to_owned(), input.surface_id.clone()))
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
        let registry = render_registry(&self.linked, states, Some(&input.surface_id))?;
        let theme = bmux_presenter::PresentationTheme::for_deck(&self.deck);
        let component = bmux_presenter::SlideComponent::new(
            slide,
            context.state.reveal_step,
            &theme,
            &registry,
        )
        .rich_content(&self.rich)
        .focused_block(&block.id);
        let event = if input.input == "click" {
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(input.x, input.y),
            ))
        } else {
            let mut chars = input.input.chars();
            let character = chars.next().ok_or(HostError::Invalid)?;
            if chars.next().is_some() {
                return Err(HostError::Invalid);
            }
            Event::Key(bmux_keyboard::KeyStroke::simple(
                bmux_keyboard::KeyCode::Char(character),
            ))
        };
        let outcome = component.event(
            &event,
            &geometry.layout,
            &mut EventCx::with_clip(
                &geometry.layout,
                bmux_tui::geometry::Rect::new(0, 0, 80, 24),
            ),
        );
        self.layouts
            .remove(&(context.instance.to_owned(), input.surface_id.clone()));
        Ok(OwnerUpdate {
            outcome: if outcome == EventOutcome::Ignored {
                "input outside active control"
            } else {
                "component updated"
            }
            .into(),
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
            .get(&(context.instance.to_owned(), id.to_owned()))
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
                        surface_revision: frame.revision,
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
