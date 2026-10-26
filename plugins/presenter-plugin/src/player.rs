//! Caller-local native playback; scheduling, input admission and image presentation
//! are exclusively delegated to bmux_tui_runtime.
use anyhow::{Context, Result, ensure};
use bmux_keyboard::KeyCode;
use bmux_presenter::component::SpeakerNotesComponent;
use bmux_presenter::custom::CustomRegistry;
use bmux_presenter::theme::PresentationTheme;
use bmux_presenter::viewer::{ViewerComponent, ViewerState};
use bmux_presenter::{Deck, NavigationAction, PresentationState, reduce};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Component, Constraints, LayoutCx, LocalRect, PaintCx, Size};
use bmux_tui::terminal::Terminal;
use bmux_tui_runtime::{
    ImageTerminalPresenter, Program, Runtime, RuntimeConfig, RuntimeEvent, TerminalInput, Update,
};
use std::io::{IsTerminal, stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) struct RemoteConnection {
    pub actions: tokio::sync::mpsc::Sender<bmux_presenter_runtime::Action>,
    pub updates: tokio::sync::mpsc::Receiver<Result<crate::viewer::RemoteUpdate, String>>,
    pub receive_only: bool,
}

fn is_playbook_block(blocks: &[bmux_presenter::Block], id: &str) -> bool {
    blocks.iter().any(|block| match &block.kind {
        bmux_presenter::BlockKind::Custom { name, .. } => {
            block.id.as_str() == id && name == "playbook-demo"
        }
        bmux_presenter::BlockKind::Columns { columns } => columns
            .iter()
            .any(|column| is_playbook_block(&column.blocks, id)),
        _ => false,
    })
}

struct LocalFactory {
    factory: std::sync::Arc<dyn bmux_presenter::custom::LinkedComponentFactory>,
    states: std::collections::BTreeMap<String, bmux_presenter::custom::LinkedState>,
    interactions: std::collections::BTreeMap<String, bmux_presenter::custom::LinkedState>,
}
impl bmux_presenter::custom::CustomComponentFactory for LocalFactory {
    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        use std::hash::{Hash, Hasher};
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for (id, state) in &self.states {
            id.hash(&mut paint);
            if let Ok(value) = state.lock() {
                value.to_string().hash(&mut paint);
            }
        }
        bmux_tui::component::ComponentRevision::new(0, paint.finish())
    }
    fn build<'a>(
        &'a self,
        block: &'a bmux_presenter::Block,
    ) -> Result<bmux_tui::component::Element<'a>, String> {
        self.build_with_context(
            block,
            bmux_presenter::custom::PresentationBuildContext::default(),
        )
    }
    fn focusable(&self, block: &bmux_presenter::Block) -> bool {
        self.factory.focusable(block)
    }
    fn build_with_context<'a>(
        &'a self,
        block: &'a bmux_presenter::Block,
        mut context: bmux_presenter::custom::PresentationBuildContext,
    ) -> Result<bmux_tui::component::Element<'a>, String> {
        let state = self
            .states
            .get(block.id.as_str())
            .ok_or("missing prepared component state")?;
        context.interaction = self.interactions.get(block.id.as_str()).cloned();
        self.factory
            .build_with_context(block, std::sync::Arc::clone(state), context)
    }
}
type LocalStates = std::collections::BTreeMap<
    bmux_presenter::custom::CompatibilityKey,
    bmux_presenter::custom::LinkedState,
>;
fn prepare_states(
    deck: &Deck,
    linked: &bmux_presenter::custom::LinkedRegistry,
    previous: &LocalStates,
) -> Result<LocalStates, String> {
    fn collect(
        blocks: &[bmux_presenter::Block],
        linked: &bmux_presenter::custom::LinkedRegistry,
        previous: &LocalStates,
        states: &mut LocalStates,
    ) -> Result<(), String> {
        for block in blocks {
            if let Some(key) = linked.compatibility_key(block) {
                let state = if let Some(state) = previous.get(&key) {
                    let value = state
                        .lock()
                        .map_err(|_| "component state mutex poisoned")?
                        .clone();
                    std::sync::Arc::new(std::sync::Mutex::new(value))
                } else {
                    std::sync::Arc::new(std::sync::Mutex::new(
                        linked
                            .factory(&key.component_type)
                            .ok_or("missing linked factory")?
                            .initial_state(block)?,
                    ))
                };
                // Validate construction before replacing the live deck or state map.
                linked
                    .factory(&key.component_type)
                    .ok_or("missing linked factory")?
                    .build(block, std::sync::Arc::clone(&state))?;
                states.insert(key, state);
            }
            if let bmux_presenter::BlockKind::Columns { columns } = &block.kind {
                for column in columns {
                    collect(&column.blocks, linked, previous, states)?;
                }
            }
        }
        Ok(())
    }
    let mut states = LocalStates::new();
    for slide in &deck.slides {
        collect(&slide.blocks, linked, previous, &mut states)?;
    }
    Ok(states)
}

#[derive(Clone)]
struct CommittedDemo {
    id: String,
    owner_epoch: uuid::Uuid,
    generation: Option<u64>,
}

/// Caller-owned arrangement; only successful output promotes pending input geometry.
#[derive(Default)]
struct PresentationView {
    painted_demo: Option<CommittedDemo>,
    pending_demo: Option<CommittedDemo>,
    painted_hits: bmux_tui::hit::HitMap,
    pending_hits: bmux_tui::hit::HitMap,
    pending_evidence: Option<serde_json::Value>,
    evidence_revision: u64,
    viewer: ViewerState,
    painted_viewer: ViewerState,
    pending_viewer: ViewerState,
    interactions: std::cell::RefCell<LocalStates>,
    focused_block: Option<bmux_presenter::BlockId>,
    managed_focus_nonce: u64,
    pending_managed_focus: Option<crate::managed_input::FocusAck>,
    managed_focus_started: Option<Instant>,
    painted_focus: Option<bmux_presenter::BlockId>,
    pending_focus: Option<bmux_presenter::BlockId>,
    painted_layout: Option<bmux_tui::component::LayoutNode>,
    pending_layout: Option<bmux_tui::component::LayoutNode>,
    mirror_geometry: Vec<MirrorGeometry>,
    pending_mirror_geometry: Vec<MirrorGeometry>,
    navigation: Option<Option<bmux_presenter::SlideId>>,
    displayed: Option<DisplayedContent>,
    pending_content: Option<DisplayedContent>,
}
struct DisplayedContent {
    deck: Deck,
    state: PresentationState,
    rich: crate::rich::RichAssets,
    surfaces: Vec<bmux_presenter_runtime::mirror::MirrorFrame>,
}
type MirrorGeometry = (
    String,
    u64,
    String,
    bmux_presenter_runtime::mirror::CanvasPlacement,
);
impl PresentationView {
    fn commit(&mut self) {
        self.displayed = self.pending_content.take();
        self.painted_demo = self.pending_demo.take();
        self.painted_hits = std::mem::take(&mut self.pending_hits);
        self.painted_layout = self.pending_layout.take();
        self.painted_focus = self.pending_focus.take();
        self.painted_viewer
            .scroll
            .set(self.pending_viewer.scroll.get());
        self.viewer.scroll.set(self.pending_viewer.scroll.get());
        self.mirror_geometry = std::mem::take(&mut self.pending_mirror_geometry);
    }
}

pub(crate) struct Player {
    pub _playbook_approval: Option<crate::playbook::Approval>,
    pub(crate) live_demos: Option<crate::playbook_live::LiveDemos>,
    local_view: PresentationView,
    browser_view: PresentationView,
    linked: bmux_presenter::custom::LinkedRegistry,
    instances: LocalStates,
    pub rich: crate::rich::RichAssets,
    pub surfaces: Vec<bmux_presenter_runtime::mirror::MirrorFrame>,
    producer_run: Option<String>,
    producer_generation: u64,
    reloads: Option<tokio::sync::mpsc::Receiver<Result<(Deck, crate::rich::RichAssets), String>>>,
    pub remote: Option<RemoteConnection>,
    pub deck: Deck,
    pub state: PresentationState,
    pub presenter: bool,
    pub notes: Option<String>,
    source: Option<PathBuf>,
    last_source: Option<std::time::SystemTime>,
    pub status: Option<String>,
    pub browser: Option<std::sync::Arc<std::sync::Mutex<crate::web::Browser>>>,
    pub stop: Option<tokio::sync::watch::Receiver<bool>>,
    browser_publication_error: Option<String>,
    audience_size: Size,
}

impl Player {
    pub(crate) fn audience_epoch(&self) -> (u64, Vec<(&str, uuid::Uuid)>) {
        let demos = self.live_demos.as_ref().map_or_else(Vec::new, |demos| {
            demos
                .entries
                .iter()
                .filter(|(_, demo)| demo.open)
                .map(|(id, demo)| (id.as_str(), demo.snapshot.owner_epoch))
                .collect()
        });
        (self.producer_generation, demos)
    }

    pub fn local(deck: Deck, source: Option<PathBuf>, presenter: bool) -> Self {
        let state = PresentationState::new(&deck);
        let last_source = source
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok()?.modified().ok());
        let linked = crate::demo::linked_registry();
        let prepared = prepare_states(&deck, &linked, &LocalStates::new());
        let (instances, status) = match prepared {
            Ok(states) => (states, None),
            Err(error) => (
                LocalStates::new(),
                Some(format!("Component preparation rejected: {error}")),
            ),
        };
        Self {
            _playbook_approval: None,
            live_demos: None,
            local_view: PresentationView::default(),
            browser_view: PresentationView::default(),
            linked,
            instances,
            rich: crate::rich::RichAssets::default(),
            surfaces: vec![],
            producer_run: None,
            producer_generation: 0,
            reloads: None,
            remote: None,
            deck,
            state,
            presenter,
            notes: None,
            source,
            last_source,
            status,
            browser: None,
            stop: None,
            browser_publication_error: None,
            audience_size: Size::new(80, 24),
        }
    }
    /// The transport run identity, not a view revision, replaces producer epochs.
    pub(crate) fn observe_producer_run(&mut self, run: &str) -> Result<()> {
        if self.producer_run.as_deref() != Some(run) {
            self.producer_generation = self
                .producer_generation
                .checked_add(1)
                .context("producer generation exhausted")?;
            self.producer_run = Some(run.to_owned());
        }
        Ok(())
    }

    fn registry(&self) -> CustomRegistry<'static> {
        self.registry_for_view(None)
    }
    fn registry_for_view(&self, view: Option<&PresentationView>) -> CustomRegistry<'static> {
        let mut registry = CustomRegistry::new();
        if self.remote.is_some() {
            return registry;
        }
        if let Some(view) = view {
            view.interactions
                .borrow_mut()
                .retain(|key, _| self.instances.contains_key(key));
        }
        let mut grouped = std::collections::BTreeMap::<
            String,
            std::collections::BTreeMap<String, bmux_presenter::custom::LinkedState>,
        >::new();
        for (key, state) in &self.instances {
            grouped
                .entry(key.component_type.clone())
                .or_default()
                .insert(key.id.clone(), std::sync::Arc::clone(state));
        }
        for (name, states) in grouped {
            if let Some(factory) = self.linked.factory(&name) {
                registry
                    .register(
                        name.clone(),
                        LocalFactory {
                            factory: std::sync::Arc::clone(factory),
                            interactions: states
                                .keys()
                                .filter_map(|id| {
                                    let view = view?;
                                    let key = self
                                        .instances
                                        .keys()
                                        .find(|key| key.id == *id && key.component_type == name)?;
                                    let state = view
                                        .interactions
                                        .borrow_mut()
                                        .entry(key.clone())
                                        .or_insert_with(|| {
                                            std::sync::Arc::new(std::sync::Mutex::new(
                                                serde_json::json!({}),
                                            ))
                                        })
                                        .clone();
                                    Some((id.clone(), state))
                                })
                                .collect(),
                            states,
                        },
                    )
                    .expect("grouped linked names are unique and nonempty");
            }
        }
        registry
    }
    fn route_mirror(&self, event: &Event) -> Result<bool> {
        self.route_mirror_target(event, None)
    }
    fn route_mirror_target(&self, event: &Event, launcher: Option<&str>) -> Result<bool> {
        let Some(remote) = &self.remote else {
            return Ok(false);
        };
        if remote.receive_only {
            return Ok(false);
        }
        let selected_surface = self.surfaces.iter().find(|surface| {
            launcher.is_some_and(|id| id == surface.surface_id)
                || self
                    .local_view
                    .painted_focus
                    .as_ref()
                    .is_some_and(|id| id.as_str() == surface.surface_id)
        });
        let geometry = match event {
            Event::Mouse(mouse) => {
                self.local_view
                    .mirror_geometry
                    .iter()
                    .rev()
                    .find(|(_, _, _, placement)| {
                        placement
                            .input_point(mouse.position.x, mouse.position.y)
                            .is_some()
                    })
            }
            Event::Key(_) | Event::Paste(_) => {
                if launcher.is_none()
                    && self.local_view.focused_block != self.local_view.painted_focus
                {
                    return Ok(false);
                }
                selected_surface.and_then(|surface| {
                    self.local_view
                        .mirror_geometry
                        .iter()
                        .find(|(id, _, _, _)| *id == surface.surface_id)
                })
            }
            _ => None,
        };
        let Some((surface_id, revision, slide_id, placement)) = geometry else {
            return Ok(false);
        };
        if self
            .state
            .slide_id
            .as_ref()
            .is_none_or(|slide| slide.as_str() != slide_id)
        {
            return Ok(false);
        }
        if self.surfaces.iter().all(|surface| {
            surface.surface_id != *surface_id
                || surface.private_scene.as_ref().map_or(
                    surface.interaction_revision.unwrap_or(surface.revision),
                    |scene| scene.frame.revision,
                ) != *revision
        }) {
            return Ok(false);
        }
        if self.local_view.painted_demo.is_some()
            && let Some(scene) = self
                .surfaces
                .iter()
                .find(|surface| surface.surface_id == *surface_id)
                .and_then(|surface| surface.private_scene.as_ref())
            && let Event::Key(key) = event
        {
            let token = match overlay_key_input(key).as_deref() {
                Some("r") => "run",
                Some("p") => "pause",
                Some("c") => "resume",
                Some("s") => "stop",
                Some("R") => "restart",
                Some("n") => "step",
                Some("escape") => "close",
                _ => return Ok(false),
            };
            let target_token = if matches!(token, "run" | "pause" | "resume") {
                "play-toggle"
            } else {
                token
            };
            let Some(target) = scene
                .interactions
                .iter()
                .find(|target| target.input == target_token && target.enabled)
            else {
                return Ok(false);
            };
            remote
                .actions
                .try_send(bmux_presenter_runtime::Action::DemoInput(
                    bmux_presenter_runtime::SurfaceInput {
                        surface_id: surface_id.clone(),
                        surface_revision: *revision,
                        x: target.area.x,
                        y: target.area.y,
                        input: token.into(),
                    },
                ))
                .context("managed modal key admission full or disconnected")?;
            return Ok(true);
        }
        let playbook = self
            .state
            .current_slide(&self.deck)
            .is_some_and(|slide| is_playbook_block(&slide.blocks, surface_id));
        let (x, y, input) = if playbook {
            match event {
                Event::Key(key) => {
                    let Some(input) = overlay_key_input(key) else {
                        return Ok(false);
                    };
                    (0, 0, input)
                }
                Event::Mouse(mouse)
                    if mouse.kind
                        == bmux_tui::event::MouseEventKind::Down(
                            bmux_tui::event::MouseButton::Left,
                        ) =>
                {
                    let Some((x, y)) = placement.input_point(mouse.position.x, mouse.position.y)
                    else {
                        return Ok(false);
                    };
                    (x, y, "click".to_owned())
                }
                _ => return Ok(false),
            }
        } else {
            let Some(input) = crate::managed_input::encode(event) else {
                return Ok(false);
            };
            let (x, y) = match event {
                Event::Mouse(mouse) => {
                    let Some(point) = placement.input_point(mouse.position.x, mouse.position.y)
                    else {
                        return Ok(false);
                    };
                    point
                }
                _ => (0, 0),
            };
            (x, y, input)
        };
        if playbook {
            if !matches!(
                input.as_str(),
                "o" | "r" | "s" | "R" | "p" | "n" | "c" | "escape" | "click"
            ) {
                return Ok(false);
            }
            remote
                .actions
                .try_send(bmux_presenter_runtime::Action::DemoInput(
                    bmux_presenter_runtime::SurfaceInput {
                        surface_id: surface_id.clone(),
                        surface_revision: *revision,
                        x,
                        y,
                        input: if input == "o" {
                            "open".into()
                        } else {
                            input.clone()
                        },
                    },
                ))
                .context("playbook effect admission full or disconnected")?;
            return Ok(true);
        }
        remote
            .actions
            .try_send(bmux_presenter_runtime::Action::DemoInput(
                bmux_presenter_runtime::SurfaceInput {
                    surface_id: surface_id.clone(),
                    surface_revision: *revision,
                    x,
                    y,
                    input,
                },
            ))
            .context("demo input admission full or disconnected")?;
        Ok(true)
    }
    fn focus_targets(&self, registry: &CustomRegistry<'_>) -> Vec<bmux_presenter::BlockId> {
        fn collect(
            blocks: &[bmux_presenter::Block],
            step: u32,
            registry: &CustomRegistry<'_>,
            out: &mut Vec<bmux_presenter::BlockId>,
        ) {
            for block in blocks.iter().filter(|block| block.reveal_step <= step) {
                match &block.kind {
                    bmux_presenter::BlockKind::Custom { .. } if registry.focusable(block) => {
                        out.push(block.id.clone())
                    }
                    bmux_presenter::BlockKind::Columns { columns } => {
                        for column in columns {
                            collect(&column.blocks, step, registry, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut targets = vec![];
        if self.remote.is_some() {
            if let Some(slide) = &self.state.slide_id {
                for surface in &self.surfaces {
                    if surface.slide_id == slide.as_str()
                        && surface.focusable
                        && !targets
                            .iter()
                            .any(|id: &bmux_presenter::BlockId| id.as_str() == surface.surface_id)
                    {
                        targets.push(surface.surface_id.clone().into());
                    }
                }
            }
            return targets;
        }
        if let Some(slide) = self.state.current_slide(&self.deck) {
            collect(
                &slide.blocks,
                self.state.reveal_step,
                registry,
                &mut targets,
            );
        }
        targets
    }
    fn route_managed_focus_key(&mut self, key: bmux_keyboard::KeyStroke) -> Result<bool> {
        let Some(remote) = &self.remote else {
            return Ok(false);
        };
        if remote.receive_only {
            return Ok(false);
        }
        if self.local_view.pending_managed_focus.is_some() {
            return Ok(true);
        }
        let Some(id) = &self.local_view.painted_focus else {
            return Ok(false);
        };
        if self.local_view.focused_block.as_ref() != Some(id) {
            return Ok(true);
        }
        if self
            .state
            .current_slide(&self.deck)
            .is_some_and(|slide| is_playbook_block(&slide.blocks, id.as_str()))
        {
            return Ok(false);
        }
        let Some((surface_id, revision, slide_id, _)) = self
            .local_view
            .mirror_geometry
            .iter()
            .find(|(surface, _, _, _)| surface == id.as_str())
        else {
            return Ok(true);
        };
        if self
            .state
            .slide_id
            .as_ref()
            .is_none_or(|id| id.as_str() != slide_id)
            || !self.surfaces.iter().any(|surface| {
                surface.surface_id == *surface_id
                    && surface.private_scene.is_none()
                    && surface.interaction_revision.unwrap_or(surface.revision) == *revision
            })
        {
            return Ok(true);
        }
        self.local_view.managed_focus_nonce = self
            .local_view
            .managed_focus_nonce
            .checked_add(1)
            .context("managed focus request identity exhausted")?;
        let input = crate::managed_input::encode_focus(key, self.local_view.managed_focus_nonce)
            .context("invalid managed focus key")?;
        let ack = crate::managed_input::focus_ack(surface_id, *revision, &input, false)
            .context("invalid managed focus request")?;
        crate::managed_input::encode_ack(&ack).context("focus acknowledgment too large")?;
        let input = bmux_presenter_runtime::SurfaceInput {
            surface_id: surface_id.clone(),
            surface_revision: *revision,
            x: 0,
            y: 0,
            input,
        };
        remote
            .actions
            .try_send(bmux_presenter_runtime::Action::DemoInput(input))
            .context("managed focus admission full or disconnected")?;
        self.local_view.pending_managed_focus = Some(ack);
        self.local_view.managed_focus_started = Some(Instant::now());
        Ok(true)
    }
    fn apply_managed_focus_ack(&mut self, wire: &str) -> bool {
        let Some(ack) = crate::managed_input::decode_ack(wire) else {
            return false;
        };
        let Some(pending) = &self.local_view.pending_managed_focus else {
            return false;
        };
        if ack.surface_id != pending.surface_id
            || ack.revision != pending.revision
            || ack.request != pending.request
        {
            return false;
        }
        self.local_view.pending_managed_focus = None;
        self.local_view.managed_focus_started = None;
        if !ack.handled
            && self
                .local_view
                .focused_block
                .as_ref()
                .is_some_and(|id| id.as_str() == ack.surface_id)
        {
            match crate::managed_input::decode(&ack.request, 0, 0) {
                Some(Event::Key(key)) if key.key == KeyCode::Tab => {
                    self.cycle_focus(key.modifiers.shift)
                }
                Some(Event::Key(key)) if key.key == KeyCode::Escape => {
                    self.local_view.focused_block = None
                }
                _ => {}
            }
        }
        true
    }
    fn cycle_focus(&mut self, reverse: bool) {
        let registry = self.registry();
        let targets = self.focus_targets(&registry);
        // Include slide navigation in the focus ring, even for one control.
        let current = self
            .local_view
            .focused_block
            .as_ref()
            .and_then(|id| targets.iter().position(|target| target == id));
        self.local_view.focused_block = match (current, reverse) {
            (None, false) => targets.first().cloned(),
            (None, true) => targets.last().cloned(),
            (Some(0), true) => None,
            (Some(index), true) => targets.get(index - 1).cloned(),
            (Some(index), false) => targets.get(index + 1).cloned(),
        };
    }

    fn route_component(&mut self, event: &Event) -> bmux_tui::event::EventOutcome {
        if matches!(event, Event::Key(_) | Event::Paste(_))
            && self.local_view.focused_block != self.local_view.painted_focus
        {
            return bmux_tui::event::EventOutcome::Ignored;
        }
        let Some(layout) = &self.local_view.painted_layout else {
            return bmux_tui::event::EventOutcome::Ignored;
        };
        let Some(displayed) = &self.local_view.displayed else {
            return bmux_tui::event::EventOutcome::Ignored;
        };
        let theme = PresentationTheme::for_deck(&displayed.deck);
        let registry = self.registry_for_view(Some(&self.local_view));
        let committed_scroll = self.local_view.painted_viewer.scroll.get();
        let mut component = ViewerComponent::new(
            &displayed.deck,
            &displayed.state,
            &self.local_view.painted_viewer,
            &theme,
            &registry,
        )
        .rich_content(&displayed.rich);
        if let Some(id) = &self.local_view.painted_focus
            && self.local_view.focused_block.as_ref() == Some(id)
        {
            component = component.focused_block(id);
        }
        let mirror = displayed
            .surfaces
            .iter()
            .rev()
            .find(|surface| {
                displayed
                    .state
                    .slide_id
                    .as_ref()
                    .is_some_and(|slide| slide.as_str() == surface.slide_id)
                    && self
                        .local_view
                        .painted_focus
                        .as_ref()
                        .is_none_or(|id| id.as_str() == surface.surface_id)
            })
            .and_then(|surface| {
                crate::mirror::MirrorComponent::new(surface, 16_384, 2 * 1024 * 1024).ok()
            });
        if let Some(mirror) = &mirror {
            component = component.content(mirror);
        }
        let outcome = component.event(
            event,
            layout,
            &mut bmux_tui::component::EventCx::new(layout),
        );
        self.local_view
            .viewer
            .scroll
            .set(self.local_view.painted_viewer.scroll.get());
        self.local_view.painted_viewer.scroll.set(committed_scroll);
        outcome
    }
    fn invalidate_committed_input(&mut self) {
        self.local_view.painted_layout = None;
        self.local_view.pending_layout = None;
        self.local_view.painted_demo = None;
        self.local_view.painted_hits = bmux_tui::hit::HitMap::new();
        self.local_view.pending_hits = bmux_tui::hit::HitMap::new();
        self.local_view.pending_evidence = None;
        self.local_view.pending_demo = None;
        self.local_view.focused_block = None;
        self.local_view.pending_managed_focus = None;
        self.local_view.mirror_geometry.clear();
        self.local_view.pending_mirror_geometry.clear();
    }
    fn publish_browser(&mut self) {
        if let Some(browser) = self.browser.clone() {
            let size = self.audience_size;
            let result = match browser.lock() {
                Ok(mut browser) => browser.publish(self, size.width, size.height),
                Err(_) => Err(anyhow::anyhow!("browser publication lock poisoned")),
            };
            match result {
                Ok(()) => {
                    if self.status == self.browser_publication_error {
                        self.status = None;
                    }
                    self.browser_publication_error = None;
                }
                Err(error) => {
                    let diagnostic = bounded_publication_error(&error);
                    if self.browser_publication_error.as_ref() != Some(&diagnostic) {
                        eprintln!("{diagnostic}");
                    }
                    self.status = Some(diagnostic.clone());
                    self.browser_publication_error = Some(diagnostic);
                }
            }
        }
    }

    fn reload(&mut self) -> bool {
        let previous_status = self.status.clone();
        let mut changed = false;
        if let Some(updates) = &mut self.reloads {
            for _ in 0..16 {
                let Ok(update) = updates.try_recv() else {
                    break;
                };
                match update {
                    Ok((deck, rich)) => {
                        if self.deck == deck
                            && self.rich.images == rich.images
                            && self.rich.code == rich.code
                        {
                            self.status = None;
                            continue;
                        }
                        let mut demos = match self
                            .live_demos
                            .as_ref()
                            .map(|demos| demos.prepare_reload(&deck))
                            .transpose()
                        {
                            Ok(demos) => demos,
                            Err(error) => {
                                self.status = Some(format!(
                                    "Reload rejected; retaining last valid deck: {error}"
                                ));
                                continue;
                            }
                        };
                        let states = match prepare_states(&deck, &self.linked, &self.instances) {
                            Ok(states) => states,
                            Err(error) => {
                                self.status = Some(format!(
                                    "Reload rejected; retaining last valid deck: {error}"
                                ));
                                continue;
                            }
                        };
                        if let (Some(current), Some(candidate)) = (&mut self.live_demos, &mut demos)
                        {
                            if let Err(error) = current.admit_reload(candidate) {
                                self.status = Some(error);
                                continue;
                            }
                            current.commit_reload(candidate);
                        }
                        self.live_demos = demos;
                        self.instances = states;
                        self.deck = deck;
                        self.rich = rich;
                        self.local_view.pending_layout = None;
                        self.local_view.pending_demo = None;
                        self.local_view.focused_block = None;
                        self.local_view.pending_mirror_geometry.clear();
                        self.local_view.viewer.reset_scroll();
                        reduce(&self.deck, &mut self.state, NavigationAction::Reconcile);
                        self.status = None;
                        changed = true;
                    }
                    Err(error) => {
                        self.status = Some(format!(
                            "Reload rejected; retaining last valid deck: {error}"
                        ))
                    }
                }
            }
        }
        changed || self.status != previous_status
    }
}

pub(crate) enum Message {
    InputError(std::io::Error),
    Interrupted,
}
impl Program for Player {
    type Message = Message;
    type Error = anyhow::Error;
    fn presentation_committed(
        &mut self,
        _report: bmux_tui_runtime::PresentReport,
    ) -> Update<Message> {
        self.local_view.commit();
        if let Some(mut evidence) = self.local_view.pending_evidence.take() {
            self.local_view.evidence_revision = self.local_view.evidence_revision.saturating_add(1);
            evidence["revision"] = self.local_view.evidence_revision.into();
            if let Err(error) = crate::player_evidence::publish(&evidence) {
                self.status = Some(format!("Presenter frame evidence failed: {error}"));
                eprintln!("Presenter frame evidence failed: {error}");
            }
        }
        self.publish_browser();
        Update::none()
    }
    fn update(&mut self, event: RuntimeEvent<Message>) -> Result<Update<Message>> {
        match event {
            RuntimeEvent::Message(Message::Interrupted) => Ok(Update::exit()),
            RuntimeEvent::Message(Message::InputError(error)) => Err(error.into()),
            RuntimeEvent::Timer(_) => {
                let previous_status = self.status.clone();
                let mut changed = self.reload();
                if self.local_view.pending_managed_focus.is_some()
                    && self
                        .local_view
                        .managed_focus_started
                        .is_some_and(|started| started.elapsed() >= Duration::from_secs(5))
                {
                    self.local_view.pending_managed_focus = None;
                    self.local_view.managed_focus_started = None;
                    self.status =
                        Some("Managed focus acknowledgment timed out; focus retained".into());
                    changed = true;
                }
                if let Some(demos) = &mut self.live_demos {
                    changed |= demos.update();
                }
                if self.remote.is_some() {
                    for _ in 0..16 {
                        // Release the connection borrow before applying owner state.
                        let received = self.remote.as_mut().map(|remote| remote.updates.try_recv());
                        let Some(received) = received else { break };
                        let update = match received {
                            Ok(update) => update,
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                if self.browser.is_some() {
                                    return Ok(Update::exit());
                                }
                                break;
                            }
                        };
                        match update {
                            Ok(update) => {
                                let snapshot = update.snapshot;
                                if snapshot.surfaces.iter().any(|surface| {
                                    surface.validate(16_384, 2 * 1024 * 1024).is_err()
                                }) {
                                    self.status = Some("Update rejected; retaining last valid deck: invalid mirror scene".into());
                                    continue;
                                }
                                let notes = if self.presenter { snapshot.notes } else { None };
                                let focus_epoch_changed = self.producer_run.as_deref()
                                    != Some(&snapshot.cursor.run_id)
                                    || self.deck != snapshot.deck;
                                changed |= self.producer_run.as_deref()
                                    != Some(&snapshot.cursor.run_id)
                                    || self.deck != snapshot.deck
                                    || self.state != snapshot.state
                                    || self.rich.images != update.rich.images
                                    || self.rich.code != update.rich.code
                                    || serde_json::to_vec(&self.surfaces)?
                                        != serde_json::to_vec(&snapshot.surfaces)?
                                    || self.notes != notes;
                                let previous_slide = self.state.slide_id.clone();
                                self.observe_producer_run(&snapshot.cursor.run_id)?;
                                self.deck = snapshot.deck;
                                self.state = snapshot.state;
                                if previous_slide != self.state.slide_id || focus_epoch_changed {
                                    self.local_view.pending_managed_focus = None;
                                    self.local_view.viewer.reset_scroll();
                                    self.local_view.focused_block = None;
                                }
                                self.rich = update.rich;
                                self.surfaces = snapshot.surfaces;
                                self.notes = notes;
                                self.status = update
                                    .acknowledgment
                                    .filter(|wire| !self.apply_managed_focus_ack(wire));
                            }
                            Err(error) => {
                                self.local_view.pending_managed_focus = None;
                                self.status = Some(format!(
                                    "Update rejected; retaining last valid deck: {error}"
                                ));
                            }
                        }
                    }
                }
                // Retry rejected browser admission without forcing an idle local
                // repaint. Successful retained publications suppress themselves.
                if !changed && self.browser_publication_error.is_some() {
                    self.publish_browser();
                }
                Ok(if changed || self.status != previous_status {
                    Update::redraw()
                } else {
                    Update::none()
                })
            }
            RuntimeEvent::Terminal(Event::Key(key)) => {
                if self.local_view.pending_managed_focus.is_some() {
                    return Ok(Update::none());
                }
                // Failed playbook cleanup must not trap the caller. Ordinary
                // focused editors still own q/Ctrl-C; they are not global exits.
                if self.local_view.painted_demo.is_some()
                    && (key.key == KeyCode::Char('q')
                        || (key.modifiers.ctrl && key.key == KeyCode::Char('c')))
                {
                    return Ok(Update::exit());
                }
                if let Some(id) = self.local_view.painted_demo.clone()
                    && let Some(demos) = &mut self.live_demos
                {
                    let input = overlay_key_input(&key);
                    if let Some(input) = input {
                        self.status = Some(
                            demos
                                .input_committed(&id.id, id.owner_epoch, id.generation, &input)
                                .unwrap_or_else(|error| error),
                        );
                        return Ok(Update::redraw());
                    }
                }
                if self.local_view.painted_demo.is_some() {
                    if self.remote.is_some() {
                        let _ = self.route_mirror(&Event::Key(key))?;
                    }
                    return Ok(Update::redraw());
                }
                if key.key == KeyCode::Char('o') && key.modifiers == bmux_keyboard::Modifiers::NONE
                {
                    let registry = self.registry();
                    let demos: Vec<_> = self
                        .focus_targets(&registry)
                        .into_iter()
                        .filter(|id| {
                            self.state
                                .current_slide(&self.deck)
                                .is_some_and(|slide| is_playbook_block(&slide.blocks, id.as_str()))
                        })
                        .collect();
                    let target = self
                        .local_view
                        .focused_block
                        .as_ref()
                        .filter(|id| demos.contains(id))
                        .cloned()
                        .or_else(|| {
                            (self.local_view.focused_block.is_none() && demos.len() == 1)
                                .then(|| demos[0].clone())
                        });
                    if let Some(id) = target.filter(|_| {
                        self.local_view
                            .displayed
                            .as_ref()
                            .is_some_and(|shown| shown.state == self.state)
                    }) {
                        if self.remote.is_some() {
                            let _ =
                                self.route_mirror_target(&Event::Key(key), Some(id.as_str()))?;
                        } else if let Some(demos) = &mut self.live_demos {
                            self.status = Some(
                                demos
                                    .input(id.as_str(), "open", false)
                                    .unwrap_or_else(|error| error),
                            );
                        }
                        return Ok(Update::redraw());
                    }
                }
                // Give a focused control's overlay first refusal; otherwise Escape
                // releases viewer-local focus without changing shared component state.
                if key.key == KeyCode::Escape
                    && key.modifiers == bmux_keyboard::Modifiers::NONE
                    && self.local_view.focused_block.is_some()
                {
                    if self.route_managed_focus_key(key)? {
                        return Ok(Update::redraw());
                    }
                    if self.route_component(&Event::Key(key))
                        != bmux_tui::event::EventOutcome::Ignored
                    {
                        return Ok(Update::redraw());
                    }
                    self.local_view.focused_block = None;
                    self.local_view.pending_demo = None;
                    return Ok(Update::redraw());
                }
                if key.key == KeyCode::Tab
                    && key.modifiers
                        == (bmux_keyboard::Modifiers {
                            shift: key.modifiers.shift,
                            ..Default::default()
                        })
                {
                    if self.local_view.focused_block.is_some()
                        && self.route_managed_focus_key(key)?
                    {
                        return Ok(Update::redraw());
                    }
                    if self.local_view.focused_block.is_some()
                        && self.route_component(&Event::Key(key))
                            != bmux_tui::event::EventOutcome::Ignored
                    {
                        return Ok(Update::redraw());
                    }
                    self.cycle_focus(key.modifiers.shift);
                    self.local_view.pending_demo = None;
                    return Ok(Update::redraw());
                }
                if self.remote.is_some()
                    && self.local_view.painted_focus.is_some()
                    && self.local_view.focused_block == self.local_view.painted_focus
                    && self.route_mirror(&Event::Key(key))?
                {
                    return Ok(Update::redraw());
                }
                // A focused editor owns editing keys, even when it ignores them.
                // Only Escape/Tab releases focus; ignored control keys never navigate.
                if self.local_view.focused_block.is_some() {
                    let _ = self.route_component(&Event::Key(key));
                    return Ok(Update::redraw());
                }
                if (key.modifiers == bmux_keyboard::Modifiers::NONE
                    && matches!(key.key, KeyCode::Char('q') | KeyCode::Escape))
                    || (key.modifiers.ctrl && key.key == KeyCode::Char('c'))
                {
                    return Ok(Update::exit());
                }
                ensure!(
                    !(key.modifiers.ctrl && key.key == KeyCode::Char('z')),
                    "outer job suspension is unsupported; exiting with terminal restoration"
                );
                if self.route_component(&Event::Key(key)) != bmux_tui::event::EventOutcome::Ignored
                {
                    return Ok(Update::redraw());
                }
                if key.modifiers != bmux_keyboard::Modifiers::NONE {
                    return Ok(Update::none());
                }
                let action = match key.key {
                    KeyCode::Right
                    | KeyCode::Down
                    | KeyCode::PageDown
                    | KeyCode::Enter
                    | KeyCode::Char(' ') => Some(NavigationAction::Next),
                    KeyCode::Left | KeyCode::Up | KeyCode::PageUp | KeyCode::Backspace => {
                        Some(NavigationAction::Previous)
                    }
                    KeyCode::Home => Some(NavigationAction::First),
                    KeyCode::End => Some(NavigationAction::Last),
                    _ => None,
                };
                if let Some(action) = action {
                    if let Some(remote) = &self.remote {
                        if !remote.receive_only {
                            remote
                                .actions
                                .try_send(bmux_presenter_runtime::Action::Navigate(action))
                                .context("navigation admission full or disconnected")?;
                        }
                    } else {
                        let previous_slide = self.state.slide_id.clone();
                        reduce(&self.deck, &mut self.state, action);
                        if previous_slide != self.state.slide_id {
                            if let (Some(demos), Some(slide)) =
                                (&mut self.live_demos, &previous_slide)
                            {
                                demos.leave(slide.as_str()).map_err(anyhow::Error::msg)?;
                            }
                            self.local_view.viewer.reset_scroll();
                            self.local_view.focused_block = None;
                        }
                    }
                }
                Ok(Update::redraw())
            }
            RuntimeEvent::Terminal(event) => {
                if self.local_view.pending_managed_focus.is_some() {
                    return Ok(Update::none());
                }
                if matches!(&event, Event::Paste(text) if text.len() > crate::managed_input::MAX_PASTE_BYTES)
                {
                    return Ok(Update::none());
                }
                // A displayed local modal never forwards pointer/paste to its
                // obscured slide. Closing takes effect after the next commit.
                if let Some(demo) = self.local_view.painted_demo.clone() {
                    if let Event::Mouse(mouse) = &event
                        && mouse.kind
                            == bmux_tui::event::MouseEventKind::Down(
                                bmux_tui::event::MouseButton::Left,
                            )
                        && let Some(hit) = self.local_view.painted_hits.hit_mouse(*mouse)
                        && let Some(input) =
                            crate::playbook_component::target_input(hit.id().as_str())
                    {
                        if let Some(demos) = &mut self.live_demos {
                            self.status = Some(
                                demos
                                    .input_committed(
                                        &demo.id,
                                        demo.owner_epoch,
                                        demo.generation,
                                        input,
                                    )
                                    .unwrap_or_else(|error| error),
                            );
                        } else if let Some(remote) = &self.remote
                            && !remote.receive_only
                            && let Some((surface_id, revision, _, placement)) =
                                self.local_view.mirror_geometry.first()
                            && let Some((x, y)) =
                                placement.input_point(mouse.position.x, mouse.position.y)
                        {
                            remote
                                .actions
                                .try_send(bmux_presenter_runtime::Action::DemoInput(
                                    bmux_presenter_runtime::SurfaceInput {
                                        surface_id: surface_id.clone(),
                                        surface_revision: *revision,
                                        x,
                                        y,
                                        input: input.into(),
                                    },
                                ))
                                .context("managed modal input admission full or disconnected")?;
                        }
                        return Ok(Update::redraw());
                    }
                    return Ok(Update::none());
                }
                let pointer_focus = if let Event::Mouse(mouse) = &event
                    && mouse.kind
                        == bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
                {
                    if self.remote.is_some() {
                        self.local_view
                            .mirror_geometry
                            .iter()
                            .rev()
                            .find(|(_, _, _, placement)| {
                                placement
                                    .input_point(mouse.position.x, mouse.position.y)
                                    .is_some()
                            })
                            .and_then(|(id, _, _, _)| {
                                self.surfaces
                                    .iter()
                                    .find(|surface| surface.surface_id == *id && surface.focusable)
                            })
                            .map(|surface| bmux_presenter::BlockId::new(&surface.surface_id))
                    } else {
                        let registry = self.registry();
                        self.local_view
                            .painted_hits
                            .hit_mouse(*mouse)
                            .and_then(|hit| {
                                let hit = hit.id().as_str();
                                self.focus_targets(&registry)
                                    .into_iter()
                                    .filter(|id| {
                                        hit == id.as_str()
                                            || hit.strip_prefix(id.as_str()).is_some_and(|suffix| {
                                                suffix.starts_with('.') || suffix.starts_with('/')
                                            })
                                    })
                                    .max_by_key(|id| id.as_str().len())
                            })
                    }
                } else {
                    None
                };
                let launch = pointer_focus.as_ref().filter(|id| {
                    matches!(&event, Event::Mouse(mouse) if self.local_view.painted_hits.hit_mouse(*mouse)
                        .is_some_and(|hit| hit.id().as_str() == format!("{}/launch", id.as_str())))
                });
                if !self.route_mirror(&event)? {
                    if let (Some(id), Some(demos)) = (launch, &mut self.live_demos) {
                        self.status = Some(
                            demos
                                .input(id.as_str(), "open", false)
                                .unwrap_or_else(|error| error),
                        );
                    } else {
                        let _ = self.route_component(&event);
                    }
                }
                if let Some(id) = pointer_focus {
                    self.local_view.focused_block = Some(id);
                }
                Ok(Update::redraw())
            }
        }
    }
}

fn clipped_mirror_placement(
    placement: bmux_presenter_runtime::mirror::CanvasPlacement,
    viewport: bmux_tui::component::LogicalRect,
    scroll_x: u64,
    scroll_y: u64,
) -> Option<bmux_presenter_runtime::mirror::CanvasPlacement> {
    // Intersect in content coordinates before mapping to the visible viewport.
    let left = u64::from(placement.destination_x).max(scroll_x);
    let top = u64::from(placement.destination_y).max(scroll_y);
    let right = (u64::from(placement.destination_x) + u64::from(placement.width))
        .min(scroll_x.saturating_add(viewport.width));
    let bottom = (u64::from(placement.destination_y) + u64::from(placement.height))
        .min(scroll_y.saturating_add(viewport.height));
    if left >= right || top >= bottom {
        return None;
    }
    Some(bmux_presenter_runtime::mirror::CanvasPlacement {
        source_x: u16::try_from(
            u64::from(placement.source_x) + left - u64::from(placement.destination_x),
        )
        .ok()?,
        source_y: u16::try_from(
            u64::from(placement.source_y) + top - u64::from(placement.destination_y),
        )
        .ok()?,
        destination_x: u16::try_from(viewport.x + left - scroll_x).ok()?,
        destination_y: u16::try_from(viewport.y + top - scroll_y).ok()?,
        width: u16::try_from(right - left).ok()?,
        height: u16::try_from(bottom - top).ok()?,
    })
}

fn paint(player: &mut Player, cx: &mut PaintCx<'_, '_>) {
    let theme = PresentationTheme::for_deck(&player.deck);
    // Notes and status are outside ViewerComponent, but share its deck background.
    // Inheritance also keeps row-clearing text writes from erasing that background.
    cx.with_style(theme.background, |cx| {
        cx.fill(cx.area(), " ", theme.background);
        paint_content(player, &theme, cx);
    });
}

// Keep slide layout independent of the operator overlay's full-terminal bounds.
// Returning the original subtree preserves viewer geometry and event routing.
struct AudienceViewport<C> {
    component: C,
    size: Size,
}

impl<C: Component> Component for AudienceViewport<C> {
    fn layout(
        &self,
        _constraints: Constraints,
        cx: &mut LayoutCx,
    ) -> bmux_tui::component::LayoutNode {
        self.component.layout(Constraints::tight(self.size), cx)
    }

    fn paint(&self, layout: &bmux_tui::component::LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.component.paint(layout, cx);
    }

    fn revision(&self) -> bmux_tui::component::ComponentRevision {
        self.component.revision()
    }

    fn event(
        &self,
        event: &Event,
        layout: &bmux_tui::component::LayoutNode,
        cx: &mut bmux_tui::component::EventCx<'_>,
    ) -> bmux_tui::event::EventOutcome {
        self.component.event(event, layout, cx)
    }
}

/// Paint the audience tree, with local operator overlays only when requested.
/// Browser publication uses this path without private notes or local controls.
fn paint_audience(
    content: AudienceContent<'_>,
    view: &mut PresentationView,
    theme: &PresentationTheme,
    size: Size,
    show_local_focus: bool,
    cx: &mut PaintCx<'_, '_>,
) {
    let navigation = content.state.slide_id.clone();
    if view.navigation.as_ref() != Some(&navigation) {
        view.viewer.reset_scroll();
        view.focused_block = None;
        view.navigation = Some(navigation);
    }
    view.pending_viewer.scroll.set(view.viewer.scroll.get());
    let registry = content.registry;
    let targets = content.targets;
    if view
        .focused_block
        .as_ref()
        .is_some_and(|id| !targets.contains(id))
    {
        view.focused_block = None;
    }
    let mirror_surface = content.state.current_slide(content.deck).and_then(|slide| {
        content.surfaces.iter().rev().find(|surface| {
            surface.slide_id == slide.id.as_str()
                && view
                    .focused_block
                    .as_ref()
                    .is_none_or(|id| id.as_str() == surface.surface_id)
        })
    });
    let operator = if show_local_focus {
        mirror_surface
            .and_then(|surface| surface.private_scene.as_ref())
            .and_then(|scene| {
                Some(crate::mirror::OperatorMirror {
                    mirror: crate::mirror::MirrorComponent::new(
                        &scene.frame,
                        16_384,
                        2 * 1024 * 1024,
                    )
                    .ok()?,
                    scene,
                })
            })
    } else {
        None
    };
    let mirror = mirror_surface.and_then(|surface| {
        crate::mirror::MirrorComponent::new(surface, 16_384, 2 * 1024 * 1024)
            .ok()
            .map(|mirror| {
                mirror.focused(
                    show_local_focus
                        && view
                            .focused_block
                            .as_ref()
                            .is_some_and(|id| id.as_str() == surface.surface_id),
                )
            })
    });
    let mut component = ViewerComponent::new(
        content.deck,
        content.state,
        &view.pending_viewer,
        theme,
        &registry,
    )
    .rich_content(content.rich);
    if show_local_focus && let Some(id) = &view.focused_block {
        component = component.focused_block(id);
    }
    if let Some(mirror) = &mirror {
        component = component.content(mirror);
    }
    let live = content
        .state
        .slide_id
        .as_ref()
        .and_then(|slide| content.live_demos.as_ref()?.visible(slide.as_str()));
    let overlay_size = if show_local_focus {
        let area = cx.area();
        Size::new(area.width, area.height.saturating_sub(1))
    } else {
        size
    };
    let mut pending_mirror_geometry = vec![];
    let (layout, demo_id) = if let Some(operator) = &operator {
        let id = operator.scene.identity["id"].as_str().unwrap_or_default();
        let overlay =
            bmux_tui::composition::Overlay::new(AudienceViewport { component, size }, operator)
                .modal(bmux_tui::focus::FocusScopeId::new(format!("playbook-{id}")));
        let layout = overlay.layout(Constraints::tight(overlay_size), &mut LayoutCx::default());
        cx.with_child(
            0,
            0,
            LocalRect::new(0, 0, overlay_size.width, overlay_size.height),
            |child| overlay.paint(&layout, child),
        );
        if let (Some(surface), Some(foreground)) = (mirror_surface, layout.children.get(1))
            && let Some(mut placement) = operator.mirror.placement(&foreground.node)
        {
            placement.destination_x = placement
                .destination_x
                .saturating_add(u16::try_from(foreground.x).unwrap_or(u16::MAX));
            placement.destination_y = placement
                .destination_y
                .saturating_add(u16::try_from(foreground.y).unwrap_or(u16::MAX));
            pending_mirror_geometry.push((
                surface.surface_id.clone(),
                operator.scene.frame.revision,
                surface.slide_id.clone(),
                placement,
            ));
        }
        (
            layout,
            Some(CommittedDemo {
                id: id.into(),
                owner_epoch: operator.scene.identity["owner_epoch"]
                    .as_str()
                    .and_then(|value| uuid::Uuid::parse_str(value).ok())
                    .unwrap_or_default(),
                generation: operator.scene.identity["generation"].as_u64(),
            }),
        )
    } else if let Some((id, demo)) = live {
        let overlay = bmux_tui::composition::Overlay::new(
            bmux_tui::composition::StyleScope::new(
                AudienceViewport { component, size },
                bmux_tui::style::Style::new().add_modifier(bmux_tui::style::Modifier::DIM),
            ),
            demo.view(!show_local_focus),
        )
        .id(format!("playbook-overlay-{id}"))
        .modal(bmux_tui::focus::FocusScopeId::new(format!("playbook-{id}")));
        let layout = overlay.layout(Constraints::tight(overlay_size), &mut LayoutCx::default());
        cx.with_child(
            0,
            0,
            LocalRect::new(0, 0, overlay_size.width, overlay_size.height),
            |child| overlay.paint(&layout, child),
        );
        (
            layout,
            Some(CommittedDemo {
                id: id.to_owned(),
                owner_epoch: demo.snapshot.owner_epoch,
                generation: demo
                    .snapshot
                    .attempt
                    .as_ref()
                    .map(|attempt| attempt.generation),
            }),
        )
    } else {
        let layout = component.layout(Constraints::tight(size), &mut LayoutCx::default());
        cx.with_child(
            0,
            0,
            LocalRect::new(0, 0, size.width, size.height),
            |child| component.paint(&layout, child),
        );
        if let (Some(surface), Some(mirror)) = (mirror_surface, &mirror)
            && let Some((viewport, content)) = component.content_geometry(&layout)
            && let Some(placement) = mirror.placement(content).and_then(|placement| {
                clipped_mirror_placement(
                    placement,
                    viewport,
                    view.pending_viewer.scroll.get().horizontal_offset(),
                    view.pending_viewer.scroll.get().vertical_offset(),
                )
            })
        {
            pending_mirror_geometry.push((
                surface.surface_id.clone(),
                surface.interaction_revision.unwrap_or(surface.revision),
                surface.slide_id.clone(),
                placement,
            ));
        }
        (layout, None)
    };
    view.pending_demo = demo_id;
    view.pending_mirror_geometry = pending_mirror_geometry;
    view.pending_focus = view.focused_block.clone();
    view.pending_layout = Some(layout);
}

fn paint_content(player: &mut Player, theme: &PresentationTheme, cx: &mut PaintCx<'_, '_>) {
    let area = cx.area();
    let notes_height =
        if player.presenter && std::env::var_os("BMUX_PRESENTER_FRAME_EVIDENCE").is_none() {
            area.height / 3
        } else {
            0
        };
    let slide_height = area.height.saturating_sub(notes_height).saturating_sub(1);
    let audience_size = Size::new(area.width, slide_height);
    player.audience_size = audience_size;
    let Some(slide) = player.state.current_slide(&player.deck) else {
        paint_view(player, theme, audience_size, true, cx);
        return;
    };
    if player.presenter {
        // Local notes only. Remote notes are supplied by an authorized Host projection.
        let mut projected = slide.clone();
        if player.remote.is_some() {
            projected.notes.text = player.notes.clone().unwrap_or_default();
        } else if let Some(notes) = &player.notes {
            projected.notes.text.clone_from(notes);
        }
        let notes = SpeakerNotesComponent {
            slide: &projected,
            theme,
        };
        let layout = notes.layout(
            Constraints::tight(Size::new(area.width, notes_height)),
            &mut LayoutCx::default(),
        );
        cx.with_child(
            0,
            i64::from(slide_height),
            LocalRect::new(0, 0, area.width, notes_height),
            |child| notes.paint(&layout, child),
        );
    }
    let footer = player.status.clone().unwrap_or_else(|| {
        format!(
            "{}  reveal {}  ←/→ navigate · ↑/↓/PgUp/PgDn scroll · Tab focus · Esc unfocus · q quit{}",
            slide.id.as_str(),
            player.state.reveal_step,
            if player.presenter {
                " · PRIVATE PRESENTER"
            } else {
                ""
            }
        )
    });
    cx.write_line(
        LocalRect::new(0, i64::from(area.height.saturating_sub(1)), area.width, 1),
        &bmux_tui::text::Line::raw(footer),
    );
    // Paint operator overlays above private notes, without resizing the slide.
    paint_view(player, theme, audience_size, true, cx);
}

pub(crate) fn paint_browser_audience(player: &mut Player, cx: &mut PaintCx<'_, '_>) {
    let theme = PresentationTheme::for_deck(&player.deck);
    let area = cx.area();
    cx.fill(area, " ", theme.background);
    paint_view(
        player,
        &theme,
        Size::new(area.width, area.height),
        false,
        cx,
    );
}

pub(crate) fn commit_browser_view(player: &mut Player) {
    player.browser_view.commit();
}

pub(crate) fn audience_surfaces(
    player: &mut Player,
    generation: &str,
    revision: u64,
) -> Result<Vec<bmux_presenter::audience::AudienceSurface>> {
    fn collect(blocks: &[bmux_presenter::Block], reveal: u32, ids: &mut Vec<String>) {
        for block in blocks.iter().filter(|block| block.reveal_step <= reveal) {
            match &block.kind {
                bmux_presenter::BlockKind::Custom { .. } => ids.push(block.id.as_str().to_owned()),
                bmux_presenter::BlockKind::Columns { columns } => {
                    for column in columns {
                        collect(&column.blocks, reveal, ids);
                    }
                }
                _ => {}
            }
        }
    }
    let mut ids = vec![];
    if let Some(slide) = player.state.current_slide(&player.deck) {
        collect(&slide.blocks, player.state.reveal_step, &mut ids);
    }
    if player.remote.is_some() {
        return player
            .surfaces
            .iter()
            .filter(|surface| {
                player
                    .state
                    .slide_id
                    .as_ref()
                    .is_some_and(|slide| slide.as_str() == surface.slide_id)
                    && ids.contains(&surface.surface_id)
            })
            .map(|surface| {
                let retained = surface.retained_surface(
                    player.producer_generation,
                    16_384,
                    2 * 1024 * 1024,
                )?;
                let component = bmux_tui::embedded_surface::EmbeddedSurface::new(&retained);
                let size = Size::new(surface.width, surface.height);
                let layout = component.layout(Constraints::tight(size), &mut LayoutCx::default());
                let scene = bmux_tui::presentation::PresentationState::new(Rect::new(
                    0,
                    0,
                    size.width,
                    size.height,
                ));
                let prepared = scene
                    .prepare(bmux_tui::damage::Damage::Full, |cx| {
                        component.paint(&layout, cx)
                    })
                    .context("missing surface preparation")?;
                Ok(bmux_presenter::audience::AudienceSurface {
                    block_id: surface.surface_id.clone(),
                    playbook: surface.playbook.clone(),
                    frame: bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
                        generation, revision, &prepared,
                    )?,
                })
            })
            .collect();
    }
    let registry = player.registry();
    fn find<'a>(
        blocks: &'a [bmux_presenter::Block],
        id: &str,
    ) -> Option<&'a bmux_presenter::Block> {
        for block in blocks {
            if block.id.as_str() == id {
                return Some(block);
            }
            if let bmux_presenter::BlockKind::Columns { columns } = &block.kind {
                for column in columns {
                    if let Some(block) = find(&column.blocks, id) {
                        return Some(block);
                    }
                }
            }
        }
        None
    }
    ids.into_iter()
        .map(|block_id| {
            let slide = player
                .state
                .current_slide(&player.deck)
                .context("missing custom slide")?;
            let block = find(&slide.blocks, &block_id).context("missing custom block")?;
            let component = if let Some(demo) = player
                .live_demos
                .as_ref()
                .and_then(|demos| demos.entries.get(&block_id))
            {
                if demo.open {
                    demo.view(true)
                } else {
                    registry.build(block).map_err(anyhow::Error::msg)?
                }
            } else {
                registry.build(block).map_err(anyhow::Error::msg)?
            };
            let size = player
                .live_demos
                .as_ref()
                .and_then(|demos| demos.entries.get(&block_id))
                .filter(|demo| demo.open)
                .map_or(Size::new(80, 24), |demo| {
                    let area = demo
                        .snapshot
                        .surface
                        .as_ref()
                        .map(|c| c.surface.buffer().area());
                    Size::new(
                        area.map_or(demo.preview_size.width, |a| a.width)
                            .saturating_add(2)
                            .min(240),
                        area.map_or(demo.preview_size.height, |a| a.height)
                            .saturating_add(8)
                            .min(128),
                    )
                });
            let layout = component.layout(Constraints::tight(size), &mut LayoutCx::default());
            let scene = bmux_tui::presentation::PresentationState::new(Rect::new(
                0,
                0,
                size.width,
                size.height,
            ));
            let prepared = scene
                .prepare_bounded(
                    bmux_tui::presentation::PresentationLimits {
                        max_cells: 16_384,
                        max_entries: 16_384,
                        max_bytes: 2 * 1024 * 1024,
                    },
                    bmux_tui::damage::Damage::Full,
                    |cx| component.paint(&layout, cx),
                )?
                .context("missing custom preparation")?;
            let playbook = player
                .live_demos
                .as_ref()
                .and_then(|demos| demos.entries.get(&block_id))
                .filter(|demo| demo.open)
                .map(|demo| demo.audience_playbook(generation, revision))
                .transpose()
                .map_err(anyhow::Error::msg)?;
            Ok(bmux_presenter::audience::AudienceSurface {
                block_id,
                playbook,
                frame: bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
                    generation, revision, &prepared,
                )?,
            })
        })
        .collect()
}

struct AudienceContent<'a> {
    deck: &'a Deck,
    state: &'a PresentationState,
    rich: &'a crate::rich::RichAssets,
    surfaces: &'a [bmux_presenter_runtime::mirror::MirrorFrame],
    live_demos: Option<&'a crate::playbook_live::LiveDemos>,
    registry: CustomRegistry<'static>,
    targets: Vec<bmux_presenter::BlockId>,
}

fn paint_view(
    player: &mut Player,
    theme: &PresentationTheme,
    size: Size,
    local: bool,
    cx: &mut PaintCx<'_, '_>,
) {
    let registry = player.registry_for_view(if local {
        Some(&player.local_view)
    } else {
        None
    });
    let targets = player.focus_targets(&registry);
    let content = AudienceContent {
        deck: &player.deck,
        state: &player.state,
        rich: &player.rich,
        surfaces: &player.surfaces,
        live_demos: player.live_demos.as_ref(),
        registry,
        targets,
    };
    let view = if local {
        &mut player.local_view
    } else {
        &mut player.browser_view
    };
    view.pending_content = Some(DisplayedContent {
        deck: player.deck.clone(),
        state: player.state.clone(),
        rich: player.rich.clone(),
        surfaces: player.surfaces.clone(),
    });
    paint_audience(content, view, theme, size, local, cx);
}

pub(crate) async fn run(player: Player) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal() && stdout().is_terminal(),
        "presentation playback requires stdin/stdout TTY"
    );
    let mut guard = TerminalGuard::enter()?;
    let result = run_owned(player, &mut guard).await;
    let restored = guard.restore();
    match (result, restored) {
        (_, Err(error)) => Err(error.context("presentation terminal restoration failed")),
        (result, Ok(())) => result,
    }
}

fn start_reloads(player: &mut Player) -> Option<tokio::task::JoinHandle<()>> {
    let mut reload_task = None;
    if let Some(source) = player.source.take() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        player.reloads = Some(receiver);
        let mut modified = player.last_source.take();
        reload_task = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let path = source.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let next = std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok();
                    if next == modified {
                        return (next, None);
                    }
                    let result = crate::source::loaded(&path)
                        .and_then(|loaded| {
                            crate::rich::RichAssets::prepare(&loaded.assets).map(|mut rich| {
                                rich.code =
                                    std::sync::Arc::new(crate::rich::highlight(&loaded.deck));
                                (loaded.deck, rich)
                            })
                        })
                        .map_err(|error| error.to_string());
                    (next, Some(result))
                })
                .await;
                let Ok((next, update)) = result else {
                    break;
                };
                if let Some(update) = update {
                    modified = next;
                    if sender.send(update).await.is_err() {
                        break;
                    }
                }
            }
        }));
    }
    reload_task
}

pub(crate) async fn run_headless(
    mut player: Player,
    mut browser: crate::web::Browser,
) -> Result<()> {
    eprintln!(
        "Static read-only audience view; browser navigation is disabled. Ctrl-C closes the listener."
    );
    let reload_task = start_reloads(&mut player);
    let mut width = 80;
    let mut height = 24;
    let mut tick = tokio::time::interval(Duration::from_millis(300));
    let mut publication_pending = match browser.publish(&mut player, width, height) {
        Ok(()) => false,
        Err(error) => {
            eprintln!("Audience update rejected; committed scene retained: {error}");
            true
        }
    };
    let result = loop {
        let changed = tokio::select! {
            signal = tokio::signal::ctrl_c() => { break signal.map_err(anyhow::Error::from); }
            request = browser.viewport_request() => {
                if let Some(request) = request {
                    if browser.viewport_is_current(&request) {
                        let changed = width != request.cols || height != request.rows;
                        width = request.cols;
                        height = request.rows;
                        changed
                    } else { false }
                } else { break Ok(()); }
            }
            _ = tick.tick() => {
                let reload = player.reload();
                let live = player.live_demos.as_mut().is_some_and(crate::playbook_live::LiveDemos::update);
                reload || live
            }
        };
        if changed || publication_pending {
            publication_pending = match browser.publish(&mut player, width, height) {
                Ok(()) => false,
                Err(error) => {
                    if !publication_pending {
                        eprintln!("Audience update rejected; committed scene retained: {error}");
                    }
                    true
                }
            };
        }
    };
    if let Some(task) = reload_task {
        task.abort();
        let _ = task.await;
    }
    let cleanup = shutdown_local_demos(&mut player).await;
    browser.shutdown().await;
    cleanup?;
    result
}

fn install_native_selection_sources(
    player: &Player,
    prepared: &mut bmux_tui::presentation::PreparedPresentation,
) -> Result<(), bmux_tui::presentation::PresentationLimitError> {
    let mut sources = std::collections::BTreeMap::new();
    if let Some(slide) = player.state.current_slide(&player.deck) {
        sources = bmux_presenter::component::markdown_selection_sources(slide);
        sources.extend(crate::rich::code_selection_sources(slide));
    }
    let visible_ids = prepared
        .selection()
        .fragments()
        .iter()
        .map(|fragment| &fragment.content_id)
        .collect::<std::collections::BTreeSet<_>>();
    sources.retain(|id, _| visible_ids.contains(id));
    prepared.set_selection_text(sources)
}

async fn run_owned(mut player: Player, guard: &mut TerminalGuard) -> Result<()> {
    let reload_task = start_reloads(&mut player);
    let (cols, rows) = crossterm::terminal::size()?;
    let terminal = Terminal::new(stdout(), Rect::new(0, 0, cols, rows));
    let presenter =
        ImageTerminalPresenter::detect(terminal, paint, bmux_image::ImageConfig::default())
            .configure_prepared(
                |player: &mut Player,
                 prepared: &mut bmux_tui::presentation::PreparedPresentation| {
                    let scopes = player
                        .local_view
                        .pending_demo
                        .as_ref()
                        .map(|id| bmux_tui::focus::FocusScopeId::new(format!("playbook-{}", id.id)))
                        .into_iter()
                        .collect::<Vec<_>>();
                    prepared.set_modal_scopes(&scopes);
                    player.local_view.pending_hits = prepared.hits().clone();
                    player.local_view.pending_evidence = crate::player_evidence::capture(
                        prepared, player.local_view.pending_demo.as_ref().map(|demo| {
                            serde_json::json!({"id":demo.id,"owner_epoch":demo.owner_epoch.to_string(),"generation":demo.generation})
                        }));
                    if let Some(evidence) = &mut player.local_view.pending_evidence {
                        evidence["surface_inputs"] = serde_json::json!(player.local_view.pending_mirror_geometry.iter().map(|(id, revision, _, p)| {
                            serde_json::json!({"id":id,"revision":revision,"x":p.destination_x,"y":p.destination_y,"width":p.width,"height":p.height,"source_x":p.source_x,"source_y":p.source_y})
                        }).collect::<Vec<_>>());
                    }
                    install_native_selection_sources(player, prepared)
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                    prepared
                        .validate_limits(bmux_tui::presentation::PresentationLimits {
                            max_cells: 262_144,
                            max_entries: 16_384,
                            max_bytes: 32 * 1024 * 1024,
                        })
                        .map_err(|error| std::io::Error::other(error.to_string()))
                },
            );
    let stop = player.stop.take();
    let (runtime, handle) = Runtime::new(player, presenter, RuntimeConfig::default());
    let mut input = TerminalInput::start::<Player>(handle.clone(), Message::InputError);
    let tick_handle = handle.clone();
    let tick = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            tick_handle.schedule_timer(
                bmux_tui_runtime::TimerId::new("presentation.reload"),
                Instant::now(),
            );
        }
    });
    let signal_handle = handle.clone();
    let signals = tokio::spawn(async move {
        let external_stop = async {
            if let Some(mut stop) = stop {
                while !*stop.borrow() {
                    if stop.changed().await.is_err() {
                        break;
                    }
                }
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = external_stop => (),
            _ = async {
        #[cfg(unix)]
        {
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = terminate.recv() => () }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
            } => (),
        }
        let _ = signal_handle.send(Message::Interrupted).await;
    });
    let result = runtime.run().await;
    signals.abort();
    let _ = signals.await;
    input.shutdown().await;
    tick.abort();
    let _ = tick.await;
    if let Some(task) = reload_task {
        task.abort();
        let _ = task.await;
    }
    match result {
        Ok(mut output) => {
            let images = output
                .presenter
                .cleanup_images()
                .map_err(anyhow::Error::from);
            let restored = guard.restore();
            let cleanup = shutdown_local_demos(&mut output.program).await;
            restored?;
            images?;
            cleanup
        }
        Err(bmux_tui_runtime::RuntimeError::Program {
            error,
            mut output,
            cleanup_error,
        }) => {
            let images = output
                .presenter
                .cleanup_images()
                .context("image cleanup after program error");
            let restored = guard.restore();
            let cleanup = shutdown_local_demos(&mut output.program).await;
            restored?;
            images?;
            cleanup?;
            if let Some(cleanup_error) = cleanup_error {
                return Err(error.context(format!("pointer restoration failed: {cleanup_error}")));
            }
            Err(error)
        }
        Err(bmux_tui_runtime::RuntimeError::Presenter {
            error,
            mut output,
            cleanup_error,
        }) => {
            output.program.invalidate_committed_input();
            output.program.local_view.focused_block = None;
            let images = output
                .presenter
                .cleanup_images()
                .context("image cleanup after presentation error");
            let restored = guard.restore();
            let cleanup = shutdown_local_demos(&mut output.program).await;
            restored?;
            images?;
            cleanup?;
            if let Some(cleanup_error) = cleanup_error {
                return Err(anyhow::anyhow!(error)
                    .context(format!("pointer restoration failed: {cleanup_error}")));
            }
            Err(anyhow::anyhow!(error))
        }
    }
}

async fn shutdown_local_demos(player: &mut Player) -> Result<()> {
    let Some(demos) = &mut player.live_demos else {
        return Ok(());
    };
    // One deadline for all demos, not twenty seconds per failed controller.
    // The service remains authoritative for unresolved resources after UI exit.
    match tokio::time::timeout(Duration::from_secs(30), demos.shutdown()).await {
        Ok(result) => result.map_err(anyhow::Error::msg),
        Err(_) => Err(anyhow::anyhow!(
            "Presenter closed; playbook cleanup is NOT acknowledged. Owned runner must reconcile; do not restart until settled."
        )),
    }
}

struct TerminalGuard {
    raw: bool,
    screen: bool,
}
impl TerminalGuard {
    fn enter() -> Result<Self> {
        let mut guard = Self {
            raw: false,
            screen: false,
        };
        crossterm::terminal::enable_raw_mode()?;
        guard.raw = true;
        // Claim cleanup before writing: partial writes also need restoration.
        guard.screen = true;
        crossterm::execute!(
            stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableMouseCapture
        )?;
        Ok(guard)
    }
    fn restore(&mut self) -> Result<()> {
        let screen = if self.screen {
            crossterm::execute!(
                stdout(),
                crossterm::event::DisableMouseCapture,
                crossterm::event::DisableBracketedPaste,
                crossterm::cursor::Show,
                crossterm::terminal::LeaveAlternateScreen
            )
        } else {
            Ok(())
        };
        if screen.is_ok() {
            self.screen = false;
        }
        let raw = if self.raw {
            crossterm::terminal::disable_raw_mode()
        } else {
            Ok(())
        };
        if raw.is_ok() {
            self.raw = false;
        }
        screen?;
        raw?;
        Ok(())
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn bounded_publication_error(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}")
        .chars()
        .filter(|character| !character.is_control())
        .take(1024)
        .collect::<String>();
    format!("Audience publication rejected; committed scene retained: {detail}")
}

/// Restore the semantic character from the canonical lowercase-plus-Shift key.
fn overlay_key_input(key: &bmux_keyboard::KeyStroke) -> Option<String> {
    match key.key {
        KeyCode::Escape => Some("escape".to_owned()),
        KeyCode::Char(character) if !key.modifiers.ctrl && !key.modifiers.alt => {
            let character = if key.modifiers.shift && character.is_ascii_alphabetic() {
                character.to_ascii_uppercase()
            } else {
                character
            };
            Some(character.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn idle_timer_and_identical_reload_do_not_invalidate() {
        let deck = bmux_presenter::DeckBuilder::new("deck")
            .slide(
                bmux_presenter::SlideBuilder::new("slide")
                    .text("body", "PUBLIC")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck.clone(), None, true);
        let tick = || RuntimeEvent::Timer(bmux_tui_runtime::TimerId::new("presentation.reload"));
        assert!(matches!(
            player.update(tick()).unwrap().invalidation,
            bmux_tui_runtime::Invalidation::None
        ));
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        player.reloads = Some(receiver);
        assert!(sender.try_send(Ok((deck, player.rich.clone()))).is_ok());
        assert!(matches!(
            player.update(tick()).unwrap().invalidation,
            bmux_tui_runtime::Invalidation::None
        ));
        assert!(sender.try_send(Err("bad reload".into())).is_ok());
        assert!(!matches!(
            player.update(tick()).unwrap().invalidation,
            bmux_tui_runtime::Invalidation::None
        ));
        assert!(sender.try_send(Err("bad reload".into())).is_ok());
        assert!(matches!(
            player.update(tick()).unwrap().invalidation,
            bmux_tui_runtime::Invalidation::None
        ));
    }

    #[test]
    fn remote_timer_ignores_cursor_only_updates_but_keeps_notes_and_epochs() {
        let deck = bmux_presenter::DeckBuilder::new("deck")
            .slide(
                bmux_presenter::SlideBuilder::new("slide")
                    .text("body", "PUBLIC")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck.clone(), None, true);
        let (actions, _actions) = tokio::sync::mpsc::channel(4);
        let (updates, receiver) = tokio::sync::mpsc::channel(4);
        player.remote = Some(RemoteConnection {
            actions,
            updates: receiver,
            receive_only: true,
        });
        let mut snapshot = bmux_presenter_runtime::Snapshot {
            cursor: bmux_presenter_runtime::Cursor {
                run_id: "run".into(),
                deck_revision: 1,
                revision: 1,
            },
            deck,
            state: player.state.clone(),
            notes: None,
            surfaces: vec![],
            assets: vec![],
        };
        player.observe_producer_run("run").unwrap();
        let tick = || RuntimeEvent::Timer(bmux_tui_runtime::TimerId::new("presentation.reload"));
        for revision in 2..4 {
            snapshot.cursor.revision = revision;
            assert!(
                updates
                    .try_send(Ok(crate::viewer::RemoteUpdate {
                        snapshot: snapshot.clone(),
                        rich: player.rich.clone(),
                        acknowledgment: None,
                    }))
                    .is_ok()
            );
            assert!(matches!(
                player.update(tick()).unwrap().invalidation,
                bmux_tui_runtime::Invalidation::None
            ));
        }
        snapshot.notes = Some("PRIVATE".into());
        assert!(
            updates
                .try_send(Ok(crate::viewer::RemoteUpdate {
                    snapshot: snapshot.clone(),
                    rich: player.rich.clone(),
                    acknowledgment: None,
                }))
                .is_ok()
        );
        assert!(!matches!(
            player.update(tick()).unwrap().invalidation,
            bmux_tui_runtime::Invalidation::None
        ));
        assert_eq!(player.notes.as_deref(), Some("PRIVATE"));
        snapshot.cursor.run_id = "replacement".into();
        assert!(
            updates
                .try_send(Ok(crate::viewer::RemoteUpdate {
                    snapshot,
                    rich: player.rich.clone(),
                    acknowledgment: None,
                }))
                .is_ok()
        );
        assert!(!matches!(
            player.update(tick()).unwrap().invalidation,
            bmux_tui_runtime::Invalidation::None
        ));
        assert_eq!(player.audience_epoch().0, 2);
    }

    #[tokio::test]
    async fn playbook_launcher_opens_with_unfocused_o_or_committed_button_click() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        for presenter in [false, true] {
            for click in [false, true] {
                let fixture = crate::playbook_live::reload_tests::Fixture::new();
                let deck = crate::playbook_live::reload_tests::deck("s", "demo");
                let mut player = Player::local(deck.clone(), None, presenter);
                player.live_demos = Some(
                    crate::playbook_live::LiveDemos::prepare_local(
                        &deck,
                        &fixture.environment,
                        vec![],
                    )
                    .unwrap(),
                );
                let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 80, 24));
                // No committed output, hence no keyboard launch authority yet.
                player
                    .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                        KeyCode::Char('o'),
                    ))))
                    .unwrap();
                assert!(!player.live_demos.as_ref().unwrap().entries["demo"].open);
                let scene = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24));
                let prepared = scene
                    .prepare(bmux_tui::damage::Damage::Full, |cx| paint(&mut player, cx))
                    .unwrap();
                player.local_view.pending_hits = prepared.hits().clone();
                player.local_view.commit();
                assert!(player.local_view.focused_block.is_none());
                let event = if click {
                    let hit = player
                        .local_view
                        .painted_hits
                        .regions()
                        .iter()
                        .find(|hit| hit.id.as_str() == "demo/launch")
                        .expect("launch button hit");
                    let point = bmux_tui::geometry::Point::new(hit.area.x, hit.area.y);
                    let mouse = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), point);
                    assert!(
                        player
                            .local_view
                            .painted_hits
                            .hit_mouse(mouse)
                            .is_some_and(|hit| hit.id().as_str() == "demo/launch"),
                        "hits={:?}",
                        player.local_view.painted_hits
                    );
                    Event::Mouse(mouse)
                } else {
                    Event::Key(KeyStroke::simple(KeyCode::Char('o')))
                };
                player.update(RuntimeEvent::Terminal(event)).unwrap();
                assert!(
                    player.live_demos.as_ref().unwrap().entries["demo"].open,
                    "presenter={presenter} click={click} status={:?} focus={:?}",
                    player.status,
                    player.local_view.focused_block
                );
                assert!(
                    player.live_demos.as_ref().unwrap().entries["demo"]
                        .snapshot
                        .attempt
                        .is_none()
                );
                terminal.draw(|cx| paint(&mut player, cx)).unwrap();
                player.local_view.commit();
                assert!(player.local_view.painted_demo.is_some());
                player
                    .live_demos
                    .as_mut()
                    .unwrap()
                    .shutdown()
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn managed_playbook_launcher_routes_unfocused_o_and_button_click_to_owner() {
        use bmux_presenter_runtime::{Owner, OwnerContext, SurfaceInput};
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        for click in [false, true] {
            let fixture = crate::playbook_live::reload_tests::Fixture::new();
            let deck = crate::playbook_live::reload_tests::deck("s", "demo");
            let state = PresentationState::new(&deck);
            let cursor = bmux_presenter_runtime::Cursor {
                run_id: "run".into(),
                deck_revision: 1,
                revision: 1,
            };
            let context = || OwnerContext {
                participant: "presenter",
                instance: "shared",
                state: &state,
                cursor: &cursor,
            };
            let mut owner =
                crate::owner::LinkedOwner::new(deck.clone(), &[], crate::demo::linked_registry())
                    .unwrap();
            owner.configure_demos(fixture.environment.clone()).unwrap();
            let surfaces = owner.scenes(context()).unwrap();
            // Clicking the surrounding canvas must not open the demo.
            assert!(
                owner
                    .demo_input(
                        context(),
                        &SurfaceInput {
                            surface_id: "demo".into(),
                            surface_revision: surfaces[0]
                                .interaction_revision
                                .unwrap_or(surfaces[0].revision),
                            x: 0,
                            y: 0,
                            input: "click".into()
                        }
                    )
                    .is_err()
            );
            let mut player = Player::local(deck, None, false);
            let (send, mut actions) = tokio::sync::mpsc::channel(4);
            let (_updates, receiver) = tokio::sync::mpsc::channel(4);
            player.remote = Some(RemoteConnection {
                actions: send,
                updates: receiver,
                receive_only: false,
            });
            player.surfaces = surfaces;
            let scene = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24));
            let prepared = scene
                .prepare(bmux_tui::damage::Damage::Full, |cx| paint(&mut player, cx))
                .unwrap();
            player.local_view.pending_hits = prepared.hits().clone();
            player.local_view.commit();
            assert!(player.local_view.painted_focus.is_none());
            let event = if click {
                let placement = player.local_view.mirror_geometry[0].3;
                Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    bmux_tui::geometry::Point::new(
                        placement.destination_x + 1,
                        placement.destination_y + 2,
                    ),
                ))
            } else {
                Event::Key(KeyStroke::simple(KeyCode::Char('o')))
            };
            player
                .update(RuntimeEvent::Terminal(event.clone()))
                .unwrap();
            let bmux_presenter_runtime::Action::DemoInput(input) =
                actions.try_recv().expect("launch admitted")
            else {
                panic!("launch action");
            };
            let opened = owner.demo_input(context(), &input).unwrap();
            assert!(opened.surfaces[0].private_scene.is_some());
            assert!(opened.surfaces[0].playbook.is_some());
            assert!(actions.try_recv().is_err());
            player.remote.as_mut().unwrap().receive_only = true;
            assert!(!player.route_mirror_target(&event, Some("demo")).unwrap());
            owner.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn audience_demo_tracks_open_close_without_execution() {
        let fixture = crate::playbook_live::reload_tests::Fixture::new();
        let deck = crate::playbook_live::reload_tests::deck("s", "old");
        let mut player = Player::local(deck.clone(), None, true);
        player.live_demos = Some(
            crate::playbook_live::LiveDemos::prepare_local(&deck, &fixture.environment, vec![])
                .unwrap(),
        );
        for (input, open) in [("close", false), ("open", true), ("close", false)] {
            player
                .live_demos
                .as_mut()
                .unwrap()
                .input("demo", input, true)
                .unwrap();
            let surfaces = audience_surfaces(&mut player, "test", 1).unwrap();
            let text = surfaces[0]
                .frame
                .buffer()
                .cells()
                .iter()
                .map(|c| c.symbol.as_str())
                .collect::<String>();
            assert_eq!(text.contains("Preview"), open);
            assert_eq!(surfaces[0].playbook.is_some(), open);
            if let Some(playbook) = &surfaces[0].playbook {
                let demo = &player.live_demos.as_ref().unwrap().entries["demo"];
                let area = playbook.terminal.buffer().area();
                assert_eq!(area.width, demo.preview_size.width);
                assert_eq!(area.height, demo.preview_size.height);
                assert!(
                    playbook
                        .terminal
                        .buffer()
                        .cells()
                        .iter()
                        .all(|cell| cell.symbol == " ")
                );
                assert!(playbook.status.contains("Terminal preview"));
                assert_eq!(playbook.progress, 0);
                playbook.validate().unwrap();
            }
            assert!(
                player.live_demos.as_ref().unwrap().entries["demo"]
                    .snapshot
                    .attempt
                    .is_none()
            );
        }
        player
            .live_demos
            .as_mut()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
    }
    #[test]
    fn native_sources_follow_clipped_visible_fragments_and_preserve_prepared_code() {
        let deck = bmux_presenter::DeckBuilder::new("sources")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .markdown("visible", "**VISIBLE_SOURCE**")
                    .code(
                        "code",
                        Some("rust".into()),
                        "let wide = \"界e\u{301}\";\nsecond line",
                    )
                    .markdown("offscreen", "OFFSCREEN_SOURCE")
                    .pause()
                    .markdown("hidden", "UNREVEALED_SOURCE")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        player.rich.code = std::sync::Arc::new(crate::rich::highlight(&player.deck));
        let scene = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 40, 30));
        let mut prepared = scene
            .prepare(bmux_tui::damage::Damage::Full, |cx| paint(&mut player, cx))
            .unwrap();
        install_native_selection_sources(&player, &mut prepared).unwrap();
        let markdown =
            bmux_tui::selection::SelectionContentId::new("presentation/s/visible/markdown");
        let code = bmux_tui::selection::SelectionContentId::new("code/code");
        assert_eq!(prepared.selection_text()[&markdown], "**VISIBLE_SOURCE**");
        assert_eq!(
            prepared.selection_text()[&code],
            "let wide = \"界e\u{301}\";\nsecond line"
        );
        assert!(
            !prepared
                .selection_text()
                .values()
                .any(|source| source == "UNREVEALED_SOURCE")
        );
        for fragment in prepared.selection().fragments() {
            let source = &prepared.selection_text()[&fragment.content_id];
            assert!(source.get(fragment.source_range.clone()).is_some());
        }
        let small = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 40, 3));
        let mut clipped = small
            .prepare(bmux_tui::damage::Damage::Full, |cx| paint(&mut player, cx))
            .unwrap();
        install_native_selection_sources(&player, &mut clipped).unwrap();
        assert!(
            !clipped
                .selection_text()
                .values()
                .any(|source| source == "OFFSCREEN_SOURCE")
        );
        assert!(clipped.selection_text().len() < prepared.selection_text().len());
        for id in clipped.selection_text().keys() {
            assert!(
                clipped
                    .selection()
                    .fragments()
                    .iter()
                    .any(|fragment| &fragment.content_id == id)
            );
        }
    }
    #[test]
    fn producer_epoch_tracks_host_run_not_view_publication() {
        let deck = bmux_presenter::DeckBuilder::new("producer")
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        player.observe_producer_run("run-a").unwrap();
        let epoch = player.producer_generation;
        player.observe_producer_run("run-a").unwrap();
        assert_eq!(player.producer_generation, epoch);
        player.observe_producer_run("run-b").unwrap();
        assert_eq!(player.producer_generation, epoch + 1);
    }
    #[test]
    fn browser_preparation_and_failed_output_leave_local_display_authority_unchanged() {
        let deck = bmux_presenter::DeckBuilder::new("views")
            .slide(
                bmux_presenter::SlideBuilder::new("s")
                    .text("body", "public")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        let mut terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 40, 12));
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        player.local_view.commit();
        let committed = player.local_view.painted_layout.as_ref().unwrap().size;
        let scroll = player.local_view.painted_viewer.scroll.get();
        let scene = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 8, 3));
        let candidate = scene
            .prepare(bmux_tui::damage::Damage::Full, |cx| {
                paint_browser_audience(&mut player, cx)
            })
            .unwrap();
        assert_eq!(
            player.local_view.painted_layout.as_ref().unwrap().size,
            committed
        );
        assert_eq!(player.local_view.painted_viewer.scroll.get(), scroll);
        assert!(player.browser_view.painted_layout.is_none());
        assert!(player.browser_view.pending_layout.is_some());
        drop(candidate); // rejected output: deliberately no view commit
        assert!(player.browser_view.painted_layout.is_none());
        assert!(player.local_view.displayed.is_some());
    }

    #[test]
    fn uncommitted_navigation_keeps_displayed_content_and_focus() {
        let deck = bmux_presenter::DeckBuilder::new("views")
            .slide(
                bmux_presenter::SlideBuilder::new("a")
                    .text("one", "first")
                    .build(),
            )
            .slide(
                bmux_presenter::SlideBuilder::new("b")
                    .text("two", "second")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        let mut terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 40, 12));
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        player.local_view.commit();
        reduce(&player.deck, &mut player.state, NavigationAction::Next);
        let scene = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 40, 12));
        let _candidate = scene.prepare(bmux_tui::damage::Damage::Full, |cx| paint(&mut player, cx));
        assert_eq!(
            player.local_view.displayed.as_ref().unwrap().state.slide_id,
            Some("a".into())
        );
        assert_eq!(
            player
                .local_view
                .pending_content
                .as_ref()
                .unwrap()
                .state
                .slide_id,
            Some("b".into())
        );
    }
    use super::*;
    use bmux_keyboard::KeyStroke;
    use bmux_presenter::builder::{DeckBuilder, SlideBuilder};

    #[tokio::test]
    async fn local_reload_retains_active_controller_and_rejects_changed_approval_atomically() {
        use crate::playbook_live::reload_tests::{Fixture, deck, empty};
        let fixture = Fixture::new();
        let running = fixture.running();
        let epoch = running.demos.entries["demo"].controller.owner_epoch();
        let surface = running.demos.entries["demo"]
            .snapshot
            .surface
            .clone()
            .unwrap();
        let mut player = Player::local(deck("s", "old"), None, true);
        player.live_demos = Some(running.demos);
        player.local_view.focused_block = Some("demo".into());
        let (updates, receiver) = tokio::sync::mpsc::channel(4);
        player.reloads = Some(receiver);
        updates
            .try_send(Ok((
                deck("moved", "renamed"),
                crate::rich::RichAssets::default(),
            )))
            .unwrap();
        assert!(player.reload());
        assert!(player.local_view.focused_block.is_none());
        let entry = &player.live_demos.as_ref().unwrap().entries["demo"];
        assert_eq!(entry.controller.owner_epoch(), epoch);
        assert_eq!(entry.snapshot.attempt.as_ref().unwrap().generation, 41);
        assert!(std::sync::Arc::ptr_eq(
            entry.snapshot.surface.as_ref().unwrap(),
            &surface
        ));
        assert_eq!(entry.slide, "moved");
        assert!(!*running.stopped.borrow());
        assert_eq!(*running.cancelled.borrow(), 0);
        std::fs::write(
            fixture.root.join("steps.dsl"),
            "new-session\nsend-keys keys=changed\n",
        )
        .unwrap();
        updates
            .try_send(Ok((
                deck("s", "changed"),
                crate::rich::RichAssets::default(),
            )))
            .unwrap();
        let admitted_deck = player.deck.clone();
        // Rejection changes the visible diagnostic, not the admitted deck or runner.
        assert!(player.reload());
        assert!(player.status.is_some());
        assert!(
            !player.reload(),
            "an idle poll must not repeat the status redraw"
        );
        assert_eq!(player.deck, admitted_deck);
        assert_eq!(player.deck.slides[0].id.as_str(), "moved");
        let entry = &player.live_demos.as_ref().unwrap().entries["demo"];
        assert_eq!(entry.controller.owner_epoch(), epoch);
        assert_eq!(entry.snapshot.attempt.as_ref().unwrap().generation, 41);
        assert!(std::sync::Arc::ptr_eq(
            entry.snapshot.surface.as_ref().unwrap(),
            &surface
        ));
        assert_eq!(entry.slide, "moved");
        assert!(!*running.stopped.borrow());
        assert_eq!(*running.cancelled.borrow(), 0);
        updates
            .try_send(Ok((empty(), crate::rich::RichAssets::default())))
            .unwrap();
        assert!(player.reload());
        assert!(player.live_demos.as_ref().unwrap().entries.is_empty());
        assert_eq!(*running.cancelled.borrow(), 0);
        assert!(!*running.stopped.borrow());
    }

    #[test]
    fn failed_output_invalidation_clears_committed_and_pending_input_identity() {
        let mut player = Player::local(
            DeckBuilder::new("d")
                .slide(SlideBuilder::new("s").build())
                .build()
                .unwrap(),
            None,
            true,
        );
        player.local_view.focused_block = Some("demo".into());
        let identity = CommittedDemo {
            id: "demo".into(),
            owner_epoch: uuid::Uuid::new_v4(),
            generation: Some(41),
        };
        player.local_view.painted_demo = Some(identity.clone());
        player.local_view.pending_demo = Some(identity);
        player.invalidate_committed_input();
        assert!(player.local_view.painted_demo.is_none());
        assert!(player.local_view.pending_demo.is_none());
        assert!(player.local_view.focused_block.is_none());
        assert!(player.local_view.painted_layout.is_none());
        assert!(player.local_view.pending_layout.is_none());
    }

    #[tokio::test]
    async fn presenter_playbook_overlay_can_cover_notes_without_resizing_audience() {
        let fixture = crate::playbook_live::reload_tests::Fixture::new();
        let deck = crate::playbook_live::reload_tests::deck("s", "demo");
        let mut player = Player::local(deck.clone(), None, true);
        let mut environment = fixture.environment.clone();
        environment.attempts_root = fixture.root.join("unstarted-attempts");
        let mut demos =
            crate::playbook_live::LiveDemos::prepare_local(&deck, &environment, vec![]).unwrap();
        demos.input("demo", "open", true).unwrap();
        player.live_demos = Some(demos);
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 120, 45));
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        assert_eq!(player.audience_size, Size::new(120, 29));
        let layout = player.local_view.pending_layout.as_ref().unwrap();
        assert_eq!(layout.size.height, 44);
        assert_eq!(layout.children[0].node.size.height, 29);
        let modal = &layout.children[1];
        assert!(
            modal.y + modal.node.size.height > 29,
            "modal was constrained above notes"
        );
        assert!(modal.y + modal.node.size.height <= 44);
        let mut browser = Terminal::new(Vec::new(), Rect::new(0, 0, 120, 45));
        browser
            .draw(|cx| paint_browser_audience(&mut player, cx))
            .unwrap();
        assert_eq!(
            player
                .browser_view
                .pending_layout
                .as_ref()
                .unwrap()
                .size
                .height,
            45
        );
        assert_eq!(player.audience_size, Size::new(120, 29));
        player
            .live_demos
            .as_mut()
            .unwrap()
            .input("demo", "close", true)
            .unwrap();
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        assert_eq!(
            player
                .local_view
                .pending_layout
                .as_ref()
                .unwrap()
                .size
                .height,
            29
        );
        player
            .live_demos
            .as_mut()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
        assert!(!environment.attempts_root.exists());
    }

    #[test]
    fn publication_diagnostic_is_bounded_and_preserves_error_chain() {
        let error = anyhow::anyhow!("invalid metadata\u{1b}\n{}", "x".repeat(2048))
            .context("browser frame validation");
        let diagnostic = bounded_publication_error(&error);
        assert!(diagnostic.contains("browser frame validation: invalid metadata"));
        assert!(!diagnostic.chars().any(char::is_control));
        assert!(diagnostic.chars().count() <= 1088);
    }

    #[test]
    fn redacted_playbook_deck_prepares_inert_viewer_without_private_config() {
        let deck = DeckBuilder::new("public demo")
            .slide(
                SlideBuilder::new("slide")
                    .custom(
                        "live-demo",
                        "playbook-demo",
                        serde_json::json!({"playbook":"PRIVATE-SOURCE.dsl", "title":"PRIVATE-TITLE"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let public = deck.public();
        let encoded = serde_json::to_string(&public).unwrap();
        assert!(!encoded.contains("PRIVATE-SOURCE"));
        assert!(!encoded.contains("PRIVATE-TITLE"));
        let player = Player::local(public.clone(), None, false);
        assert!(player.status.is_none());
        assert!(player.live_demos.is_none());
        let registry = player.registry();
        let placeholder = registry.build(&public.slides[0].blocks[0]).unwrap();
        let _layout = placeholder.layout(
            Constraints::tight(Size::new(80, 24)),
            &mut LayoutCx::default(),
        );
        let bmux_presenter::BlockKind::Custom { config, .. } = &public.slides[0].blocks[0].kind
        else {
            panic!("custom block expected");
        };
        assert!(crate::playbook::DemoConfig::parse(config).is_err());
    }

    #[test]
    fn overlay_shift_r_selects_restart_while_lowercase_r_selects_run() {
        let run =
            KeyStroke::with_modifiers(KeyCode::Char('r'), bmux_keyboard::Modifiers::default());
        let restart = KeyStroke::with_modifiers(
            KeyCode::Char('r'),
            bmux_keyboard::Modifiers {
                shift: true,
                ..bmux_keyboard::Modifiers::default()
            },
        );
        assert_eq!(overlay_key_input(&run).as_deref(), Some("r"));
        assert_eq!(overlay_key_input(&restart).as_deref(), Some("R"));
    }

    #[test]
    fn remote_playbook_routes_shift_r_as_restart() {
        use bmux_presenter_runtime::mirror::{MirrorFrame, WireCell};
        let deck = DeckBuilder::new("d")
            .slide(
                SlideBuilder::new("s")
                    .custom("demo", "playbook-demo", serde_json::Value::Null)
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        let surface = MirrorFrame {
            focusable: true,
            focused: None,
            cursor: None,
            interaction_revision: None,
            private_scene: None,
            playbook: None,
            surface_id: "demo".into(),
            slide_id: "s".into(),
            revision: 4,
            width: 1,
            height: 1,
            cells: vec![WireCell {
                symbol: "x".into(),
                width: 1,
                continuation: false,
                fg: None,
                bg: None,
                modifiers: 0,
            }],
            images: vec![],
        };
        let placement = surface.placement(10, 5);
        player.surfaces.push(surface);
        player
            .local_view
            .mirror_geometry
            .push(("demo".into(), 4, "s".into(), placement));
        let (sender, mut actions) = tokio::sync::mpsc::channel(2);
        let (_, receiver) = tokio::sync::mpsc::channel(1);
        player.remote = Some(RemoteConnection {
            actions: sender,
            updates: receiver,
            receive_only: false,
        });
        let prepared = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24))
            .prepare(bmux_tui::damage::Damage::Full, |_| {})
            .unwrap();
        player.surfaces[0].playbook = Some(bmux_presenter::audience::AudiencePlaybook {
            title: "Managed demo".into(),
            status: "Ready".into(),
            progress: 2500,
            terminal: bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
                "audience-run",
                4,
                &prepared,
            )
            .unwrap(),
        });
        let audience = audience_surfaces(&mut player, "audience-run", 4).unwrap();
        let playbook = audience[0].playbook.as_ref().unwrap();
        assert_eq!(playbook.title, "Managed demo");
        assert_eq!(playbook.progress, 2500);
        assert_eq!(playbook.terminal.buffer().area(), Rect::new(0, 0, 80, 24));
        assert_eq!(audience[0].frame.buffer().area(), Rect::new(0, 0, 1, 1));
        player.local_view.focused_block = Some("demo".into());
        player.local_view.painted_focus = Some("demo".into());
        for (shift, expected) in [(false, "r"), (true, "R")] {
            let key = KeyStroke::with_modifiers(
                KeyCode::Char('r'),
                bmux_keyboard::Modifiers {
                    shift,
                    ..bmux_keyboard::Modifiers::default()
                },
            );
            assert!(player.route_mirror(&Event::Key(key)).unwrap());
            let bmux_presenter_runtime::Action::DemoInput(input) = actions.try_recv().unwrap()
            else {
                panic!("targeted playbook input expected");
            };
            assert_eq!(input.surface_id, "demo");
            assert_eq!(input.surface_revision, 4);
            assert_eq!(input.input, expected);
        }
    }

    #[test]
    fn mirror_input_requires_committed_geometry_and_matching_revision() {
        use bmux_presenter_runtime::mirror::{MirrorFrame, WireCell};
        let deck = DeckBuilder::new("d")
            .slide(SlideBuilder::new("s").build())
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        let surface = MirrorFrame {
            focusable: true,
            focused: None,
            cursor: None,
            interaction_revision: None,
            private_scene: None,
            playbook: None,
            surface_id: "demo".into(),
            slide_id: "s".into(),
            revision: 4,
            width: 2,
            height: 1,
            cells: vec![
                WireCell {
                    symbol: "x".into(),
                    width: 1,
                    continuation: false,
                    fg: None,
                    bg: None,
                    modifiers: 0
                };
                2
            ],
            images: vec![],
        };
        let placement = surface.placement(10, 5);
        player.surfaces.push(surface);
        let (sender, mut actions) = tokio::sync::mpsc::channel(2);
        let (_, receiver) = tokio::sync::mpsc::channel(1);
        player.remote = Some(RemoteConnection {
            actions: sender,
            updates: receiver,
            receive_only: false,
        });
        player.local_view.focused_block = Some("demo".into());
        player.local_view.painted_focus = Some("demo".into());
        let event = Event::Key(KeyStroke::simple(KeyCode::Char('+')));
        assert!(!player.route_mirror(&event).unwrap());
        player
            .local_view
            .mirror_geometry
            .push(("demo".into(), 4, "s".into(), placement));
        assert!(player.route_mirror(&event).unwrap());
        let bmux_presenter_runtime::Action::DemoInput(input) = actions.try_recv().unwrap() else {
            panic!("demo input expected");
        };
        assert_eq!(input.surface_revision, 4);
        assert!(
            matches!(crate::managed_input::decode(&input.input, input.x, input.y),
            Some(Event::Key(key)) if key.key == KeyCode::Char('+'))
        );
        player.local_view.focused_block = None;
        assert!(!player.route_mirror(&event).unwrap());
        player.local_view.focused_block = Some("demo".into());
        for event in [
            Event::Key(KeyStroke::with_modifiers(
                KeyCode::Home,
                bmux_keyboard::Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
            )),
            Event::Paste("bounded é paste".into()),
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::ScrollDown,
                bmux_tui::geometry::Point::new(placement.destination_x, placement.destination_y),
            )),
        ] {
            assert!(player.route_mirror(&event).unwrap());
            let bmux_presenter_runtime::Action::DemoInput(input) = actions.try_recv().unwrap()
            else {
                panic!("surface input");
            };
            assert!(crate::managed_input::decode(&input.input, input.x, input.y).is_some());
        }
        assert!(
            !player
                .route_mirror(&Event::Paste(
                    "x".repeat(crate::managed_input::MAX_PASTE_BYTES + 1)
                ))
                .unwrap()
        );
        player.surfaces[0].revision = 5;
        assert!(!player.route_mirror(&event).unwrap());
        player.remote.as_mut().unwrap().receive_only = true;
        assert!(!player.route_mirror(&event).unwrap());
    }
    #[test]
    fn focused_controls_keep_editing_keys_and_modified_navigation_local() {
        for key in [
            KeyStroke::simple(KeyCode::Char('q')),
            KeyStroke::simple(KeyCode::Right),
            KeyStroke::simple(KeyCode::Enter),
            KeyStroke::simple(KeyCode::Backspace),
            KeyStroke::with_modifiers(
                KeyCode::Right,
                bmux_keyboard::Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            ),
            KeyStroke::with_modifiers(
                KeyCode::Char('c'),
                bmux_keyboard::Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            ),
        ] {
            let deck = DeckBuilder::new("keys")
                .slide(
                    SlideBuilder::new("s")
                        .custom("count", "counter", serde_json::Value::Null)
                        .build(),
                )
                .slide(SlideBuilder::new("next").build())
                .build()
                .unwrap();
            let mut player = Player::local(deck, None, false);
            let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 80, 24));
            terminal.draw(|cx| paint(&mut player, cx)).unwrap();
            player.local_view.commit();
            player.local_view.focused_block = Some("count".into());
            terminal.draw(|cx| paint(&mut player, cx)).unwrap();
            player.local_view.commit();
            let update = player
                .update(RuntimeEvent::Terminal(Event::Key(key)))
                .unwrap();
            assert_eq!(update.lifecycle, bmux_tui_runtime::Lifecycle::Continue);
            assert_eq!(player.state.slide_id, Some("s".into()));
            assert_eq!(player.local_view.focused_block, Some("count".into()));
        }
        for modifiers in [
            bmux_keyboard::Modifiers {
                ctrl: true,
                ..Default::default()
            },
            bmux_keyboard::Modifiers {
                alt: true,
                ..Default::default()
            },
        ] {
            let deck = DeckBuilder::new("keys")
                .slide(SlideBuilder::new("s").build())
                .slide(SlideBuilder::new("next").build())
                .build()
                .unwrap();
            let mut player = Player::local(deck, None, false);
            player
                .update(RuntimeEvent::Terminal(Event::Key(
                    KeyStroke::with_modifiers(KeyCode::Right, modifiers),
                )))
                .unwrap();
            assert_eq!(player.state.slide_id, Some("s".into()));
        }
    }
    #[test]
    fn escape_closes_gallery_overlay_before_releasing_focus_and_shift_tab_reverses() {
        let deck = DeckBuilder::new("escape")
            .slide(
                SlideBuilder::new("s")
                    .custom(
                        "menu",
                        "tui-gallery",
                        serde_json::json!({"kind":"select-dropdown"}),
                    )
                    .custom("count", "counter", serde_json::Value::Null)
                    .build(),
            )
            .slide(SlideBuilder::new("next").build())
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 80, 24));
        let draw = |player: &mut Player, terminal: &mut Terminal<Vec<u8>>| {
            terminal.draw(|cx| paint(player, cx)).unwrap();
            player.local_view.commit();
        };
        draw(&mut player, &mut terminal);
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Tab,
            ))))
            .unwrap();
        draw(&mut player, &mut terminal);
        assert_eq!(player.local_view.focused_block, Some("menu".into()));
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Enter,
            ))))
            .unwrap();
        draw(&mut player, &mut terminal);
        let menu = player
            .local_view
            .interactions
            .borrow()
            .iter()
            .find(|(key, _)| key.id == "menu")
            .unwrap()
            .1
            .clone();
        assert_eq!(
            menu.lock().unwrap()["menu"]["native"]["open"],
            serde_json::json!(true)
        );
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Escape,
            ))))
            .unwrap();
        draw(&mut player, &mut terminal);
        assert_eq!(
            menu.lock().unwrap()["menu"]["native"]["open"],
            serde_json::json!(false)
        );
        assert_eq!(player.local_view.focused_block, Some("menu".into()));
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Escape,
            ))))
            .unwrap();
        draw(&mut player, &mut terminal);
        assert!(player.local_view.focused_block.is_none());
        let backwards = || {
            RuntimeEvent::Terminal(Event::Key(KeyStroke::with_modifiers(
                KeyCode::Tab,
                bmux_keyboard::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            )))
        };
        player.update(backwards()).unwrap();
        draw(&mut player, &mut terminal);
        assert_eq!(player.local_view.focused_block, Some("count".into()));
        player.update(backwards()).unwrap();
        draw(&mut player, &mut terminal);
        assert_eq!(player.local_view.focused_block, Some("menu".into()));
        assert_eq!(player.state.slide_id, Some("s".into()));
    }
    #[test]
    fn managed_escape_waits_for_correlated_owner_result_before_releasing_focus() {
        let deck = DeckBuilder::new("escape")
            .slide(
                SlideBuilder::new("s")
                    .custom(
                        "menu",
                        "tui-gallery",
                        serde_json::json!({"kind":"select-dropdown"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck.clone(), None, false);
        let state = PresentationState::new(&deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let context = || bmux_presenter_runtime::OwnerContext {
            participant: "p",
            instance: "shared",
            state: &state,
            cursor: &cursor,
        };
        let mut owner =
            crate::owner::LinkedOwner::new(deck, &[], crate::demo::linked_registry()).unwrap();
        let frames = owner.scenes(context()).unwrap();
        let opened = bmux_presenter_runtime::Owner::demo_input(
            &mut owner,
            context(),
            &bmux_presenter_runtime::SurfaceInput {
                surface_id: "menu".into(),
                surface_revision: frames[0].interaction_revision.unwrap(),
                x: 0,
                y: 0,
                input: crate::managed_input::encode(&Event::Key(KeyStroke::simple(KeyCode::Enter)))
                    .unwrap(),
            },
        )
        .unwrap();
        player.surfaces = opened.surfaces;
        let (sender, mut actions) = tokio::sync::mpsc::channel(2);
        let (_, receiver) = tokio::sync::mpsc::channel(1);
        player.remote = Some(RemoteConnection {
            actions: sender,
            updates: receiver,
            receive_only: false,
        });
        player.local_view.focused_block = Some("menu".into());
        player.local_view.painted_focus = Some("menu".into());
        for release in [false, true] {
            let surface = &player.surfaces[0];
            player.local_view.mirror_geometry = vec![(
                "menu".into(),
                surface.interaction_revision.unwrap(),
                "s".into(),
                surface.placement(0, 0),
            )];
            assert!(
                player
                    .route_managed_focus_key(KeyStroke::simple(KeyCode::Escape))
                    .unwrap()
            );
            assert_eq!(player.local_view.focused_block, Some("menu".into()));
            let bmux_presenter_runtime::Action::DemoInput(input) = actions.try_recv().unwrap()
            else {
                panic!("input");
            };
            let mut forged = player.local_view.pending_managed_focus.clone().unwrap();
            forged.revision += 1;
            assert!(
                !player
                    .apply_managed_focus_ack(&crate::managed_input::encode_ack(&forged).unwrap())
            );
            let update =
                bmux_presenter_runtime::Owner::demo_input(&mut owner, context(), &input).unwrap();
            assert_eq!(
                crate::managed_input::decode_ack(&update.outcome)
                    .unwrap()
                    .handled,
                !release
            );
            player.surfaces = update.surfaces;
            assert!(player.apply_managed_focus_ack(&update.outcome));
            assert_eq!(player.local_view.focused_block.is_none(), release);
            assert!(!player.apply_managed_focus_ack(&update.outcome));
        }
        // A consumed nested Tab keeps the block; only the correlated ignored
        // result hands off to presenter navigation. Reverse Tab uses the same path.
        for (handled, reverse) in [(true, false), (false, false), (false, true)] {
            player.local_view.focused_block = Some("menu".into());
            player.local_view.painted_focus = Some("menu".into());
            let surface = &player.surfaces[0];
            player.local_view.mirror_geometry = vec![(
                "menu".into(),
                surface.interaction_revision.unwrap(),
                "s".into(),
                surface.placement(0, 0),
            )];
            assert!(
                player
                    .route_managed_focus_key(KeyStroke::with_modifiers(
                        KeyCode::Tab,
                        bmux_keyboard::Modifiers {
                            shift: reverse,
                            ..Default::default()
                        }
                    ))
                    .unwrap()
            );
            let _ = actions.try_recv().unwrap();
            let mut ack = player.local_view.pending_managed_focus.clone().unwrap();
            ack.handled = handled;
            assert!(
                player.apply_managed_focus_ack(&crate::managed_input::encode_ack(&ack).unwrap())
            );
            assert_eq!(player.local_view.focused_block.is_some(), handled);
        }
    }
    #[test]
    fn local_view_interaction_handles_are_stable_but_not_shared_with_another_view() {
        let deck = DeckBuilder::new("views")
            .slide(
                SlideBuilder::new("s")
                    .custom(
                        "menu",
                        "tui-gallery",
                        serde_json::json!({"kind":"select-dropdown"}),
                    )
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        let other = PresentationView::default();
        let _ = player.registry_for_view(Some(&player.local_view));
        let _ = player.registry_for_view(Some(&other));
        let first = player
            .local_view
            .interactions
            .borrow()
            .values()
            .next()
            .unwrap()
            .clone();
        let second = other.interactions.borrow().values().next().unwrap().clone();
        assert!(!std::sync::Arc::ptr_eq(&first, &second));
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 80, 24));
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        player.local_view.commit();
        player.cycle_focus(false);
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        player.local_view.commit();
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Enter,
            ))))
            .unwrap();
        assert_eq!(
            first.lock().unwrap()["menu"]["native"]["open"],
            serde_json::json!(true)
        );
        assert_eq!(*second.lock().unwrap(), serde_json::json!({}));
        assert!(
            player
                .instances
                .values()
                .all(|state| state.lock().unwrap().get("open").is_none())
        );
        let _ = player.registry_for_view(Some(&player.local_view));
        assert!(std::sync::Arc::ptr_eq(
            &first,
            player
                .local_view
                .interactions
                .borrow()
                .values()
                .next()
                .unwrap()
        ));
        // Public/browser construction receives no transient handles.
        assert!(player.browser_view.interactions.borrow().is_empty());
    }
    #[test]
    fn static_gallery_is_not_a_local_or_managed_tab_stop() {
        let deck = DeckBuilder::new("static")
            .slide(
                SlideBuilder::new("s")
                    .custom("static", "tui-gallery", serde_json::json!({"kind":"badge"}))
                    .custom("count", "counter", serde_json::Value::Null)
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        assert_eq!(
            player.focus_targets(&player.registry()),
            vec![bmux_presenter::BlockId::new("count")]
        );
        let (sender, _) = tokio::sync::mpsc::channel(1);
        let (_, receiver) = tokio::sync::mpsc::channel(1);
        player.remote = Some(RemoteConnection {
            actions: sender,
            updates: receiver,
            receive_only: false,
        });
        let state = PresentationState::new(&player.deck);
        let cursor = bmux_presenter_runtime::Cursor {
            run_id: "r".into(),
            deck_revision: 1,
            revision: 1,
        };
        let mut owner = crate::owner::LinkedOwner::new(
            player.deck.clone(),
            &[],
            crate::demo::linked_registry(),
        )
        .unwrap();
        player.surfaces = owner
            .scenes(bmux_presenter_runtime::OwnerContext {
                participant: "p",
                instance: "shared",
                state: &state,
                cursor: &cursor,
            })
            .unwrap();
        assert!(
            !player
                .surfaces
                .iter()
                .find(|surface| surface.surface_id == "static")
                .unwrap()
                .focusable
        );
        assert_eq!(
            player.focus_targets(&player.registry()),
            vec![bmux_presenter::BlockId::new("count")]
        );
    }
    #[test]
    fn scroll_input_uses_committed_viewport_and_resets_on_slide_change() {
        let deck = DeckBuilder::new("d")
            .slide(
                SlideBuilder::new("s")
                    .text(
                        "body",
                        (0..40)
                            .map(|n| format!("line {n}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                    .build(),
            )
            .slide(SlideBuilder::new("next").build())
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 30, 10));
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        player.local_view.commit();
        assert!(
            player
                .route_component(&Event::Key(KeyStroke::simple(KeyCode::PageDown)))
                .is_handled()
        );
        assert!(player.local_view.viewer.scroll.get().vertical_offset() > 0);
        assert_eq!(
            player
                .local_view
                .painted_viewer
                .scroll
                .get()
                .vertical_offset(),
            0
        );
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Right,
            ))))
            .unwrap();
        assert_eq!(player.state.slide_id, Some("next".into()));
        assert_eq!(player.local_view.viewer.scroll.get().vertical_offset(), 0);
    }
    #[test]
    fn gallery_focus_can_be_released_without_repainting_it_back() {
        for release in [KeyCode::Escape, KeyCode::Tab] {
            for next in [KeyCode::Right, KeyCode::Char(' ')] {
                let deck = DeckBuilder::new("focus")
                    .slide(
                        SlideBuilder::new("input")
                            .custom(
                                "input",
                                "tui-gallery",
                                serde_json::json!({"kind":"text-input-box"}),
                            )
                            .build(),
                    )
                    .slide(SlideBuilder::new("next").build())
                    .build()
                    .unwrap();
                let mut player = Player::local(deck, None, false);
                let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 80, 24));
                let draw = |player: &mut Player, terminal: &mut Terminal<Vec<u8>>| {
                    terminal.draw(|cx| paint(player, cx)).unwrap();
                    player.local_view.commit();
                };
                draw(&mut player, &mut terminal);
                assert!(player.local_view.painted_focus.is_none());
                player
                    .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                        KeyCode::Tab,
                    ))))
                    .unwrap();
                draw(&mut player, &mut terminal);
                assert_eq!(player.local_view.painted_focus, Some("input".into()));
                for key in [KeyCode::Char(' '), KeyCode::Right] {
                    player
                        .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(key))))
                        .unwrap();
                    draw(&mut player, &mut terminal);
                    assert_eq!(player.state.slide_id, Some("input".into()));
                }
                let update = player
                    .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                        release,
                    ))))
                    .unwrap();
                assert_eq!(update.lifecycle, bmux_tui_runtime::Lifecycle::Continue);
                assert!(
                    !player
                        .route_component(&Event::Key(KeyStroke::simple(KeyCode::Char(' '))))
                        .is_handled()
                );
                draw(&mut player, &mut terminal);
                assert!(player.local_view.painted_focus.is_none());
                player
                    .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(next))))
                    .unwrap();
                assert_eq!(player.state.slide_id, Some("next".into()));
            }
        }
    }

    #[test]
    fn component_focus_ring_includes_navigation_in_both_directions() {
        let deck = DeckBuilder::new("focus")
            .slide(
                SlideBuilder::new("s")
                    .custom("one", "counter", serde_json::json!({}))
                    .custom("two", "toggle", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        for reverse in [false, true] {
            for expected in if reverse {
                [Some("two"), Some("one"), None]
            } else {
                [Some("one"), Some("two"), None]
            } {
                player.cycle_focus(reverse);
                assert_eq!(
                    player
                        .local_view
                        .focused_block
                        .as_ref()
                        .map(|id| id.as_str()),
                    expected
                );
            }
        }
    }

    #[test]
    fn local_linked_values_follow_stable_blocks_and_focus() {
        let deck = DeckBuilder::new("d")
            .slide(
                SlideBuilder::new("s")
                    .custom("one", "counter", serde_json::json!({}))
                    .custom("two", "counter", serde_json::json!({}))
                    .custom("switch", "toggle", serde_json::json!({}))
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck.clone(), None, false);
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 40, 18));
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        player.local_view.commit();
        player.cycle_focus(false);
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        player.local_view.commit();
        assert!(
            player
                .route_component(&Event::Key(KeyStroke::simple(KeyCode::Char('+'))))
                .is_handled()
        );
        let values = player
            .instances
            .iter()
            .map(|(key, state)| (key.id.as_str(), state.lock().unwrap().clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(values["one"], serde_json::json!(1));
        assert_eq!(values["two"], serde_json::json!(0));
        assert_eq!(values["switch"], serde_json::json!(false));
        player.cycle_focus(false);
        assert_eq!(player.local_view.focused_block, Some("two".into()));
        let replacement = prepare_states(&deck, &player.linked, &player.instances).unwrap();
        assert_eq!(
            replacement
                .iter()
                .find(|(key, _)| key.id == "one")
                .unwrap()
                .1
                .lock()
                .unwrap()
                .as_i64(),
            Some(1)
        );
    }
    #[test]
    fn mirrored_click_transform_clips_scroll_and_resize() {
        use bmux_presenter_runtime::mirror::CanvasPlacement;
        use bmux_tui::component::LogicalRect;
        let source = CanvasPlacement {
            source_x: 4,
            source_y: 0,
            destination_x: 0,
            destination_y: 0,
            width: 20,
            height: 24,
        };
        let placement =
            clipped_mirror_placement(source, LogicalRect::new(2, 1, 20, 6), 0, 10).unwrap();
        assert_eq!(placement.input_point(2, 1), Some((4, 10)));
        assert_eq!(placement.input_point(21, 6), Some((23, 15)));
        assert_eq!(placement.input_point(2, 7), None);
        assert_eq!(placement.input_point(1, 1), None);
        let resized =
            clipped_mirror_placement(source, LogicalRect::new(2, 1, 10, 3), 0, 12).unwrap();
        assert_eq!(resized.input_point(2, 1), Some((4, 12)));
        assert_eq!(resized.input_point(12, 1), None);
        // Saved placement still describes the last committed frame, not the resize.
        assert_eq!(placement.input_point(21, 6), Some((23, 15)));
    }
    #[test]
    fn browser_prepared_scene_and_transport_are_audience_only() {
        use bmux_presenter::BlockKind;
        use bmux_tui::presentation::{PresentationLimits, PresentationState as Scene};
        let image = |destination: &str| BlockKind::Image {
            destination: destination.into(),
            alt: "public image".into(),
            rows: 2,
        };
        let deck = DeckBuilder::new("d")
            .slide(
                SlideBuilder::new("s")
                    .text("public", "PUBLIC")
                    .custom("counter", "counter", serde_json::json!({}))
                    .block("image", image("visible"))
                    .pause()
                    .text("hidden", "HIDDENSECRET")
                    .block("hidden-image", image("hidden"))
                    .notes("PRIVATESECRET")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, true);
        player.status = Some("PRIVATECONTROL".into());
        for id in ["visible", "hidden"] {
            std::sync::Arc::make_mut(&mut player.rich.images).insert(
                id.into(),
                bmux_tui::image::ImagePayload::Pixels {
                    bytes: vec![255, 0, 0, 255],
                    width: 1,
                    height: 1,
                    format: bmux_tui::image::ImagePixelFormat::Rgba8,
                },
            );
        }
        let scene = Scene::new(Rect::new(0, 0, 60, 30));
        let prepared = scene
            .prepare_bounded(
                PresentationLimits {
                    max_cells: 262_144,
                    max_entries: 262_144,
                    max_bytes: 32 * 1024 * 1024,
                },
                bmux_tui::damage::Damage::Full,
                |cx| paint_browser_audience(&mut player, cx),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            prepared.images().len(),
            1,
            "unrevealed images must not be exported"
        );
        let frame =
            bmux_web_renderer::BrowserFrame::from_prepared_in_generation("audience", 1, &prepared)
                .unwrap();
        let binary = bmux_web_renderer::encode_binary(&frame).unwrap();
        assert!(binary.starts_with(&[
            b'B',
            b'M',
            b'W',
            b'P',
            bmux_web_renderer::BINARY_WIRE_VERSION
        ]));
        let decoded = bmux_web_renderer::decode_binary(&binary, None).unwrap();
        let json = bmux_web_renderer::encode_json(&decoded).unwrap();
        let all_metadata = format!("{prepared:?} {json}");
        for private in [
            "PRIVATESECRET",
            "PRIVATECONTROL",
            "HIDDENSECRET",
            "hidden-image",
        ] {
            assert!(
                !all_metadata.contains(private),
                "browser payload leaked {private}"
            );
        }
        let cells = (0..30)
            .filter_map(|y| prepared.buffer().row_symbols(y))
            .collect::<String>();
        assert!(cells.contains("PUBLIC"));
        assert!(
            cells.contains("Counter"),
            "linked custom factory must render directly"
        );
        assert!(player.presenter);
    }
    #[test]
    fn audience_tree_excludes_private_status_notes_and_unrevealed_content() {
        let deck = DeckBuilder::new("d")
            .slide(
                SlideBuilder::new("s")
                    .text("public", "PUBLIC")
                    .pause()
                    .text("hidden", "HIDDENSECRET")
                    .notes("PRIVATESECRET")
                    .build(),
            )
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, true);
        player.status = Some("PRIVATECONTROL".into());
        let theme = PresentationTheme::for_deck(&player.deck);
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 40, 20));
        terminal
            .draw(|cx| {
                cx.fill(cx.area(), " ", theme.background);
                paint_view(&mut player, &theme, Size::new(40, 20), false, cx);
            })
            .unwrap();
        let cells = (0..20)
            .filter_map(|y| terminal.retained_buffer().unwrap().row_symbols(y))
            .collect::<String>();
        assert!(cells.contains("PUBLIC"));
        for private in ["PRIVATESECRET", "PRIVATECONTROL", "HIDDENSECRET"] {
            assert!(!cells.contains(private), "audience leaked {private}");
        }
        // Exporting the audience must not turn off the independent local speaker UI.
        assert!(player.presenter);
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        let local = (0..20)
            .filter_map(|y| terminal.retained_buffer().unwrap().row_symbols(y))
            .collect::<String>();
        assert!(local.contains("PRIVATESECRET"));
        assert!(local.contains("PRIVATECONTROL"));
    }
    #[test]
    fn audience_hides_notes_and_theme_changes_affect_paint() {
        let mut deck = DeckBuilder::new("d")
            .slide(
                SlideBuilder::new("s")
                    .text("body", "public")
                    .notes("PRIVATESECRET")
                    .build(),
            )
            .build()
            .unwrap();
        deck.metadata.theme = Some("light".into());
        let mut player = Player::local(deck, None, false);
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 30, 12));
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        assert!(
            !(0..12)
                .filter_map(|y| terminal.retained_buffer().unwrap().row_symbols(y))
                .collect::<String>()
                .contains("PRIVATESECRET")
        );
        let light_background = terminal
            .retained_buffer()
            .unwrap()
            .get(bmux_tui::geometry::Point::new(29, 0))
            .unwrap()
            .style
            .bg;
        assert_eq!(light_background, Some(bmux_tui::style::Color::White));
        player.deck.metadata.theme = Some("dark".into());
        assert_eq!(PresentationTheme::for_deck(&player.deck).name, "dark");
        player.presenter = true;
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        assert_eq!(
            Some(bmux_tui::style::Color::Black),
            terminal
                .retained_buffer()
                .unwrap()
                .get(bmux_tui::geometry::Point::new(29, 0))
                .unwrap()
                .style
                .bg
        );
        for point in [
            bmux_tui::geometry::Point::new(0, 7),   // private notes row
            bmux_tui::geometry::Point::new(29, 10), // unused notes space
            bmux_tui::geometry::Point::new(29, 11), // status row
        ] {
            assert_eq!(
                terminal
                    .retained_buffer()
                    .unwrap()
                    .get(point)
                    .unwrap()
                    .style
                    .bg,
                Some(bmux_tui::style::Color::Black)
            );
        }
        assert!(
            (0..12)
                .filter_map(|y| terminal.retained_buffer().unwrap().row_symbols(y))
                .collect::<String>()
                .contains("PRIVATESECRET")
        );
        player.remote = Some(RemoteConnection {
            actions: tokio::sync::mpsc::channel(1).0,
            updates: tokio::sync::mpsc::channel(1).1,
            receive_only: true,
        });
        player.notes = None;
        terminal.draw(|cx| paint(&mut player, cx)).unwrap();
        assert!(
            !(0..12)
                .filter_map(|y| terminal.retained_buffer().unwrap().row_symbols(y))
                .collect::<String>()
                .contains("PRIVATESECRET")
        );
    }
    #[test]
    fn audience_frame_is_independent_of_local_focus_and_notes() {
        let deck = DeckBuilder::new("deck")
            .slide(SlideBuilder::new("slide").text("body", "PUBLIC").build())
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, true);
        let scene = bmux_tui::presentation::PresentationState::new(Rect::new(0, 0, 80, 24));
        let prepared = scene
            .prepare(bmux_tui::damage::Damage::Full, |cx| {
                paint_browser_audience(&mut player, cx)
            })
            .unwrap();
        let before =
            bmux_web_renderer::BrowserFrame::from_prepared_in_generation("test", 1, &prepared)
                .unwrap();
        player.local_view.focused_block = Some(bmux_presenter::BlockId::new("body"));
        player.notes = Some("PRIVATE".into());
        player.status = Some("PRIVATE".into());
        let prepared = scene
            .prepare(bmux_tui::damage::Damage::Full, |cx| {
                paint_browser_audience(&mut player, cx)
            })
            .unwrap();
        let after =
            bmux_web_renderer::BrowserFrame::from_prepared_in_generation("test", 1, &prepared)
                .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn keys_use_pure_reducer_and_quit_requests_runtime_exit() {
        let deck = DeckBuilder::new("d")
            .slide(SlideBuilder::new("a").pause().build())
            .slide(SlideBuilder::new("b").build())
            .build()
            .unwrap();
        let mut player = Player::local(deck, None, false);
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Right,
            ))))
            .unwrap();
        assert_eq!(player.state.reveal_step, 1);
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Right,
            ))))
            .unwrap();
        assert_eq!(player.state.slide_id, Some("b".into()));
        let update = player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Char('q'),
            ))))
            .unwrap();
        assert_eq!(update.lifecycle, bmux_tui_runtime::Lifecycle::Exit);
    }
}
