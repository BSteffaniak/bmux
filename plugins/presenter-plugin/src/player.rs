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

struct LocalFactory {
    factory: std::sync::Arc<dyn bmux_presenter::custom::LinkedComponentFactory>,
    states: std::collections::BTreeMap<String, bmux_presenter::custom::LinkedState>,
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
        let state = self
            .states
            .get(block.id.as_str())
            .ok_or("missing prepared component state")?;
        self.factory.build(block, std::sync::Arc::clone(state))
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

pub(crate) struct Player {
    pub _playbook_approval: Option<crate::playbook::Approval>,
    pub(crate) live_demos: Option<crate::playbook_live::LiveDemos>,
    painted_demo: Option<CommittedDemo>,
    pending_demo: Option<CommittedDemo>,
    viewer: ViewerState,
    painted_viewer: ViewerState,
    pending_viewer: ViewerState,
    focused_block: Option<bmux_presenter::BlockId>,
    linked: bmux_presenter::custom::LinkedRegistry,
    instances: LocalStates,
    pub rich: crate::rich::RichAssets,
    pub surfaces: Vec<bmux_presenter_runtime::mirror::MirrorFrame>,
    painted_layout: Option<bmux_tui::component::LayoutNode>,
    pending_layout: Option<bmux_tui::component::LayoutNode>,
    mirror_geometry: Vec<(
        String,
        u64,
        String,
        bmux_presenter_runtime::mirror::CanvasPlacement,
    )>,
    pending_mirror_geometry: Vec<(
        String,
        u64,
        String,
        bmux_presenter_runtime::mirror::CanvasPlacement,
    )>,
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
            painted_demo: None,
            pending_demo: None,
            viewer: ViewerState::new(),
            painted_viewer: ViewerState::new(),
            pending_viewer: ViewerState::new(),
            focused_block: None,
            linked,
            instances,
            rich: crate::rich::RichAssets::default(),
            surfaces: vec![],
            painted_layout: None,
            pending_layout: None,
            mirror_geometry: vec![],
            pending_mirror_geometry: vec![],
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
    fn registry(&self) -> CustomRegistry<'static> {
        let mut registry = CustomRegistry::new();
        if self.remote.is_some() {
            return registry;
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
                        name,
                        LocalFactory {
                            factory: std::sync::Arc::clone(factory),
                            states,
                        },
                    )
                    .expect("grouped linked names are unique and nonempty");
            }
        }
        registry
    }
    fn route_mirror(&self, event: &Event) -> Result<bool> {
        let Some(remote) = &self.remote else {
            return Ok(false);
        };
        if remote.receive_only {
            return Ok(false);
        }
        let selected_surface = self.surfaces.iter().find(|surface| {
            self.focused_block
                .as_ref()
                .is_some_and(|id| id.as_str() == surface.surface_id)
        });
        let geometry = selected_surface
            .and_then(|surface| {
                self.mirror_geometry
                    .iter()
                    .find(|(id, _, _, _)| *id == surface.surface_id)
            })
            .or_else(|| self.mirror_geometry.first());
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
        if self
            .surfaces
            .iter()
            .all(|surface| surface.surface_id != *surface_id || surface.revision != *revision)
        {
            return Ok(false);
        }
        let (x, y, input) = match event {
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
                let Some((x, y)) = placement.input_point(mouse.position.x, mouse.position.y) else {
                    return Ok(false);
                };
                (x, y, "click".to_owned())
            }
            _ => return Ok(false),
        };
        let playbook = self.deck.slides.iter().flat_map(|slide| slide.blocks.iter()).any(|block| block.id.as_str() == surface_id && matches!(&block.kind, bmux_presenter::BlockKind::Custom { name, .. } if name == "playbook-demo"));
        if playbook {
            if !matches!(
                input.as_str(),
                "o" | "r" | "s" | "R" | "p" | "n" | "c" | "escape" | "click"
            ) {
                return Ok(false);
            }
            remote
                .actions
                .try_send(bmux_presenter_runtime::Action::Effect {
                    name: format!("playbook-demo:{surface_id}"),
                    input: input.clone(),
                })
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
                    bmux_presenter::BlockKind::Custom { name, .. } if registry.contains(name) => {
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
    fn reconcile_focus(&mut self, registry: &CustomRegistry<'_>) {
        let targets = self.focus_targets(registry);
        if self
            .focused_block
            .as_ref()
            .is_none_or(|id| !targets.contains(id))
        {
            self.focused_block = targets.first().cloned();
        }
    }
    fn cycle_focus(&mut self, reverse: bool) {
        let registry = self.registry();
        let targets = self.focus_targets(&registry);
        let next = self
            .focused_block
            .as_ref()
            .and_then(|id| targets.iter().position(|target| target == id))
            .map_or(0, |index| {
                if reverse {
                    (index + targets.len() - 1) % targets.len()
                } else {
                    (index + 1) % targets.len()
                }
            });
        self.focused_block = targets.get(next).cloned();
    }
    fn selected_surface(&self) -> Option<&bmux_presenter_runtime::mirror::MirrorFrame> {
        let slide = self.state.current_slide(&self.deck)?;
        self.surfaces.iter().rev().find(|surface| {
            surface.slide_id == slide.id.as_str()
                && self
                    .focused_block
                    .as_ref()
                    .is_none_or(|id| id.as_str() == surface.surface_id)
        })
    }
    fn route_component(&mut self, event: &Event) -> bmux_tui::event::EventOutcome {
        let Some(layout) = &self.painted_layout else {
            return bmux_tui::event::EventOutcome::Ignored;
        };
        let theme = PresentationTheme::for_deck(&self.deck);
        let registry = self.registry();
        let committed_scroll = self.painted_viewer.scroll.get();
        let mut component = ViewerComponent::new(
            &self.deck,
            &self.state,
            &self.painted_viewer,
            &theme,
            &registry,
        )
        .rich_content(&self.rich);
        if let Some(id) = &self.focused_block {
            component = component.focused_block(id);
        }
        let mirror = self.selected_surface().and_then(|surface| {
            crate::mirror::MirrorComponent::new(surface, 16_384, 512 * 1024).ok()
        });
        if let Some(mirror) = &mirror {
            component = component.content(mirror);
        }
        let outcome = component.event(
            event,
            layout,
            &mut bmux_tui::component::EventCx::new(layout),
        );
        self.viewer.scroll.set(self.painted_viewer.scroll.get());
        self.painted_viewer.scroll.set(committed_scroll);
        outcome
    }
    fn invalidate_committed_input(&mut self) {
        self.painted_layout = None;
        self.pending_layout = None;
        self.painted_demo = None;
        self.pending_demo = None;
        self.focused_block = None;
        self.mirror_geometry.clear();
        self.pending_mirror_geometry.clear();
    }
    fn reload(&mut self) -> bool {
        let mut changed = false;
        if let Some(updates) = &mut self.reloads {
            for _ in 0..16 {
                let Ok(update) = updates.try_recv() else {
                    break;
                };
                match update {
                    Ok((deck, rich)) => {
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
                        self.painted_layout = None;
                        self.pending_layout = None;
                        self.painted_demo = None;
                        self.pending_demo = None;
                        self.focused_block = None;
                        self.mirror_geometry.clear();
                        self.pending_mirror_geometry.clear();
                        self.viewer.reset_scroll();
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
        changed
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
        self.painted_demo = self.pending_demo.take();
        self.painted_layout = self.pending_layout.take();
        self.painted_viewer
            .scroll
            .set(self.pending_viewer.scroll.get());
        self.mirror_geometry = std::mem::take(&mut self.pending_mirror_geometry);
        if let Some(browser) = self.browser.clone() {
            let size = self.audience_size;
            let result = match browser.lock() {
                Ok(mut browser) => browser.publish(self, size.width, size.height),
                Err(_) => Err(anyhow::anyhow!("browser publication lock poisoned")),
            };
            match result {
                Ok(()) => self.browser_publication_error = None,
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
        Update::none()
    }
    fn update(&mut self, event: RuntimeEvent<Message>) -> Result<Update<Message>> {
        match event {
            RuntimeEvent::Message(Message::Interrupted) => Ok(Update::exit()),
            RuntimeEvent::Message(Message::InputError(error)) => Err(error.into()),
            RuntimeEvent::Timer(_) => {
                self.reload();
                if let Some(demos) = &mut self.live_demos {
                    demos.update();
                }
                if let Some(remote) = &mut self.remote {
                    for _ in 0..16 {
                        let update = match remote.updates.try_recv() {
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
                                if snapshot
                                    .surfaces
                                    .iter()
                                    .any(|surface| surface.validate(16_384, 512 * 1024).is_err())
                                {
                                    self.status = Some("Update rejected; retaining last valid deck: invalid mirror scene".into());
                                    continue;
                                }
                                let previous_slide = self.state.slide_id.clone();
                                self.deck = snapshot.deck;
                                self.state = snapshot.state;
                                if previous_slide != self.state.slide_id {
                                    self.viewer.reset_scroll();
                                    self.focused_block = None;
                                }
                                self.mirror_geometry.clear();
                                self.rich = update.rich;
                                self.surfaces = snapshot.surfaces;
                                self.painted_layout = None;
                                self.notes = if self.presenter { snapshot.notes } else { None };
                                self.status = update.acknowledgment;
                            }
                            Err(error) => {
                                self.status = Some(format!(
                                    "Update rejected; retaining last valid deck: {error}"
                                ));
                            }
                        }
                    }
                }
                Ok(Update::redraw())
            }
            RuntimeEvent::Terminal(Event::Key(key)) => {
                // Exit is a caller lifecycle action, not an overlay control. It
                // must remain available when the runner has failed cleanup.
                if key.key == KeyCode::Char('q')
                    || (key.modifiers.ctrl && key.key == KeyCode::Char('c'))
                {
                    return Ok(Update::exit());
                }
                if let Some(id) = self.painted_demo.clone()
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
                if self.painted_demo.is_some() && matches!(key.key, KeyCode::Tab | KeyCode::Enter) {
                    return Ok(Update::none());
                }
                if key.key == KeyCode::Char('o')
                    && self.remote.is_none()
                    && let (Some(demos), Some(id)) = (&mut self.live_demos, &self.focused_block)
                {
                    self.status = Some(
                        demos
                            .input(id.as_str(), "open", false)
                            .unwrap_or_else(|error| error),
                    );
                    return Ok(Update::redraw());
                }
                if self.remote.is_some() && self.route_mirror(&Event::Key(key))? {
                    return Ok(Update::redraw());
                }
                if key.key == KeyCode::Char('q')
                    || key.key == KeyCode::Escape
                    || (key.modifiers.ctrl && key.key == KeyCode::Char('c'))
                {
                    return Ok(Update::exit());
                }
                ensure!(
                    !(key.modifiers.ctrl && key.key == KeyCode::Char('z')),
                    "outer job suspension is unsupported; exiting with terminal restoration"
                );
                if self.route_mirror(&Event::Key(key))? {
                    return Ok(Update::redraw());
                }
                if key.key == KeyCode::Tab {
                    self.cycle_focus(key.modifiers.shift);
                    // Focus changes invalidate input until the replacement frame commits.
                    self.painted_layout = None;
                    self.painted_demo = None;
                    self.pending_demo = None;
                    self.mirror_geometry.clear();
                    return Ok(Update::redraw());
                }
                if self.route_component(&Event::Key(key)) != bmux_tui::event::EventOutcome::Ignored
                {
                    return Ok(Update::redraw());
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
                            self.viewer.reset_scroll();
                            self.focused_block = None;
                        }
                        self.painted_layout = None;
                        self.mirror_geometry.clear();
                    }
                }
                Ok(Update::redraw())
            }
            RuntimeEvent::Terminal(event) => {
                // A displayed local modal never forwards pointer/paste to its
                // obscured slide. Closing takes effect after the next commit.
                if self.painted_demo.is_some() {
                    return Ok(Update::none());
                }
                let _ = self.route_mirror(&event)?;
                let _ = self.route_component(&event);
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

/// Paint only the audience tree into the supplied viewport. This deliberately
/// does not call `paint_content`: speaker notes, focus decoration and local status
/// are not audience content. Browser publication must start here, never by cropping
/// the speaker's already-painted terminal frame.
fn paint_audience(
    player: &mut Player,
    theme: &PresentationTheme,
    size: Size,
    show_local_focus: bool,
    cx: &mut PaintCx<'_, '_>,
) {
    let registry = player.registry();
    player.reconcile_focus(&registry);
    let mirror_surface = player.selected_surface();
    let mirror = mirror_surface
        .and_then(|surface| crate::mirror::MirrorComponent::new(surface, 16_384, 512 * 1024).ok());
    let mut component = ViewerComponent::new(
        &player.deck,
        &player.state,
        &player.viewer,
        theme,
        &registry,
    )
    .rich_content(&player.rich);
    if show_local_focus && let Some(id) = &player.focused_block {
        component = component.focused_block(id);
    }
    if let Some(mirror) = &mirror {
        component = component.content(mirror);
    }
    let live = player
        .state
        .slide_id
        .as_ref()
        .and_then(|slide| player.live_demos.as_ref()?.visible(slide.as_str()));
    let mut pending_mirror_geometry = vec![];
    let (layout, demo_id) = if let Some((id, demo)) = live {
        let overlay = bmux_tui::composition::Overlay::new(component, demo.view(!show_local_focus))
            .id(format!("playbook-overlay-{id}"))
            .modal(bmux_tui::focus::FocusScopeId::new(format!("playbook-{id}")));
        let layout = overlay.layout(Constraints::tight(size), &mut LayoutCx::default());
        cx.with_child(
            0,
            0,
            LocalRect::new(0, 0, size.width, size.height),
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
                    player.viewer.scroll.get().horizontal_offset(),
                    player.viewer.scroll.get().vertical_offset(),
                )
            })
        {
            pending_mirror_geometry.push((
                surface.surface_id.clone(),
                surface.revision,
                surface.slide_id.clone(),
                placement,
            ));
        }
        (layout, None)
    };
    player.pending_demo = demo_id;
    player.pending_mirror_geometry = pending_mirror_geometry;
    player.pending_viewer.scroll.set(player.viewer.scroll.get());
    player.pending_layout = Some(layout);
}

fn paint_content(player: &mut Player, theme: &PresentationTheme, cx: &mut PaintCx<'_, '_>) {
    let area = cx.area();
    let notes_height = if player.presenter { area.height / 3 } else { 0 };
    let slide_height = area.height.saturating_sub(notes_height).saturating_sub(1);
    let audience_size = Size::new(area.width, slide_height);
    player.audience_size = audience_size;
    paint_audience(player, theme, audience_size, true, cx);
    let Some(slide) = player.state.current_slide(&player.deck) else {
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
            "{}  reveal {}  ←/→ navigate · ↑/↓/PgUp/PgDn scroll · Tab focus · q quit{}",
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
}

pub(crate) fn paint_browser_audience(player: &mut Player, cx: &mut PaintCx<'_, '_>) {
    // Browser preparation cannot install pending geometry for local keyboard/mouse
    // admission or alter the local speaker's scrolling/focus state.
    let pending_demo = player.pending_demo.clone();
    let pending_layout = player.pending_layout.take();
    let pending_geometry = std::mem::take(&mut player.pending_mirror_geometry);
    let pending_scroll = player.pending_viewer.scroll.get();
    let scroll = player.viewer.scroll.get();
    let focused = player.focused_block.clone();
    let theme = PresentationTheme::for_deck(&player.deck);
    let area = cx.area();
    cx.fill(area, " ", theme.background);
    paint_audience(
        player,
        &theme,
        Size::new(area.width, area.height),
        false,
        cx,
    );
    player.pending_demo = pending_demo;
    player.pending_layout = pending_layout;
    player.pending_mirror_geometry = pending_geometry;
    player.pending_viewer.scroll.set(pending_scroll);
    player.viewer.scroll.set(scroll);
    player.focused_block = focused;
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
                                rich.code = crate::rich::highlight(&loaded.deck);
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
    if let Err(error) = browser.publish(&mut player, width, height) {
        eprintln!("Audience update rejected; committed scene retained: {error}");
    }
    let result = loop {
        let changed = tokio::select! {
            signal = tokio::signal::ctrl_c() => { break signal.map_err(anyhow::Error::from); }
            request = browser.requests.recv() => {
                if let Some(request) = request {
                    if browser.requests.is_current(&request) {
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
        if changed && let Err(error) = browser.publish(&mut player, width, height) {
            eprintln!("Audience update rejected; committed scene retained: {error}");
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
                        .pending_demo
                        .as_ref()
                        .map(|id| bmux_tui::focus::FocusScopeId::new(format!("playbook-{}", id.id)))
                        .into_iter()
                        .collect::<Vec<_>>();
                    prepared.set_modal_scopes(&scopes);
                    prepared
                        .validate_limits(bmux_tui::presentation::PresentationLimits {
                            max_cells: 262_144,
                            max_entries: 16_384,
                            max_bytes: 32 * 1024 * 1024,
                        })
                        .map_err(|error| {
                            player.invalidate_committed_input();
                            std::io::Error::other(error.to_string())
                        })
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
        Err(bmux_tui_runtime::RuntimeError::Program { error, mut output }) => {
            let images = output
                .presenter
                .cleanup_images()
                .context("image cleanup after program error");
            let restored = guard.restore();
            let cleanup = shutdown_local_demos(&mut output.program).await;
            restored?;
            images?;
            cleanup?;
            Err(error)
        }
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, mut output }) => {
            output.program.invalidate_committed_input();
            output.program.focused_block = None;
            let images = output
                .presenter
                .cleanup_images()
                .context("image cleanup after presentation error");
            let restored = guard.restore();
            let cleanup = shutdown_local_demos(&mut output.program).await;
            restored?;
            images?;
            cleanup?;
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
        player.focused_block = Some("demo".into());
        let (updates, receiver) = tokio::sync::mpsc::channel(4);
        player.reloads = Some(receiver);
        updates
            .try_send(Ok((
                deck("moved", "renamed"),
                crate::rich::RichAssets::default(),
            )))
            .unwrap();
        assert!(player.reload());
        assert!(player.focused_block.is_none());
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
        assert!(!player.reload());
        assert_eq!(player.deck.slides[0].id.as_str(), "moved");
        assert_eq!(
            player.live_demos.as_ref().unwrap().entries["demo"]
                .controller
                .owner_epoch(),
            epoch
        );
        assert!(!*running.stopped.borrow());
        assert_eq!(*running.cancelled.borrow(), 0);
        updates
            .try_send(Ok((empty(), crate::rich::RichAssets::default())))
            .unwrap();
        assert!(!player.reload());
        assert_eq!(player.deck.slides[0].id.as_str(), "moved");
        assert!(*running.cancelled.borrow() > 0);
        assert!(!*running.stopped.borrow());
        running.updates.send_modify(|snapshot| {
            snapshot.cancellation_revision = *running.cancelled.borrow();
            let attempt = snapshot.attempt.as_mut().unwrap();
            attempt.phase = "stopped".into();
            attempt.cleanup_acknowledged = true;
        });
        updates
            .try_send(Ok((empty(), crate::rich::RichAssets::default())))
            .unwrap();
        assert!(player.reload());
        assert!(player.live_demos.as_ref().unwrap().entries.is_empty());
        assert!(*running.stopped.borrow());
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
        player.focused_block = Some("demo".into());
        let identity = CommittedDemo {
            id: "demo".into(),
            owner_epoch: uuid::Uuid::new_v4(),
            generation: Some(41),
        };
        player.painted_demo = Some(identity.clone());
        player.pending_demo = Some(identity);
        player.invalidate_committed_input();
        assert!(player.painted_demo.is_none());
        assert!(player.pending_demo.is_none());
        assert!(player.focused_block.is_none());
        assert!(player.painted_layout.is_none());
        assert!(player.pending_layout.is_none());
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
            .mirror_geometry
            .push(("demo".into(), 4, "s".into(), placement));
        let (sender, mut actions) = tokio::sync::mpsc::channel(2);
        let (_, receiver) = tokio::sync::mpsc::channel(1);
        player.remote = Some(RemoteConnection {
            actions: sender,
            updates: receiver,
            receive_only: false,
        });
        for (shift, expected) in [(false, "r"), (true, "R")] {
            let key = KeyStroke::with_modifiers(
                KeyCode::Char('r'),
                bmux_keyboard::Modifiers {
                    shift,
                    ..bmux_keyboard::Modifiers::default()
                },
            );
            assert!(player.route_mirror(&Event::Key(key)).unwrap());
            let bmux_presenter_runtime::Action::Effect { name, input } =
                actions.try_recv().unwrap()
            else {
                panic!("playbook effect expected");
            };
            assert_eq!(name, "playbook-demo:demo");
            assert_eq!(input, expected);
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
        let event = Event::Key(KeyStroke::simple(KeyCode::Char('+')));
        assert!(!player.route_mirror(&event).unwrap());
        player
            .mirror_geometry
            .push(("demo".into(), 4, "s".into(), placement));
        assert!(player.route_mirror(&event).unwrap());
        let bmux_presenter_runtime::Action::DemoInput(input) = actions.try_recv().unwrap() else {
            panic!("demo input expected");
        };
        assert_eq!(input.surface_revision, 4);
        player.surfaces[0].revision = 5;
        assert!(!player.route_mirror(&event).unwrap());
        player.remote.as_mut().unwrap().receive_only = true;
        assert!(!player.route_mirror(&event).unwrap());
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
        player.painted_layout = player.pending_layout.take();
        player
            .painted_viewer
            .scroll
            .set(player.pending_viewer.scroll.get());
        assert!(
            player
                .route_component(&Event::Key(KeyStroke::simple(KeyCode::PageDown)))
                .is_handled()
        );
        assert!(player.viewer.scroll.get().vertical_offset() > 0);
        assert_eq!(player.painted_viewer.scroll.get().vertical_offset(), 0);
        player
            .update(RuntimeEvent::Terminal(Event::Key(KeyStroke::simple(
                KeyCode::Right,
            ))))
            .unwrap();
        assert_eq!(player.state.slide_id, Some("next".into()));
        assert_eq!(player.viewer.scroll.get().vertical_offset(), 0);
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
        player.painted_layout = player.pending_layout.take();
        player
            .painted_viewer
            .scroll
            .set(player.pending_viewer.scroll.get());
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
        assert_eq!(player.focused_block, Some("two".into()));
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
            player.rich.images.insert(
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
        let json = bmux_web_renderer::encode_json(&frame).unwrap();
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
                paint_audience(&mut player, &theme, Size::new(40, 20), false, cx);
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
