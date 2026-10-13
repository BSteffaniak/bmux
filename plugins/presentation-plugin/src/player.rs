//! Caller-local native playback; scheduling, input admission and image presentation
//! are exclusively delegated to bmux_tui_runtime.
use anyhow::{Context, Result, ensure};
use bmux_keyboard::KeyCode;
use bmux_presentation::component::SpeakerNotesComponent;
use bmux_presentation::custom::CustomRegistry;
use bmux_presentation::theme::PresentationTheme;
use bmux_presentation::viewer::{ViewerComponent, ViewerState};
use bmux_presentation::{Deck, NavigationAction, PresentationState, reduce};
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
    pub actions: tokio::sync::mpsc::Sender<bmux_presentation_runtime::Action>,
    pub updates: tokio::sync::mpsc::Receiver<Result<crate::viewer::RemoteUpdate, String>>,
    pub receive_only: bool,
}

struct LocalFactory {
    factory: std::sync::Arc<dyn bmux_presentation::custom::LinkedComponentFactory>,
    states: std::collections::BTreeMap<String, bmux_presentation::custom::LinkedState>,
}
impl bmux_presentation::custom::CustomComponentFactory for LocalFactory {
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
        block: &'a bmux_presentation::Block,
    ) -> Result<bmux_tui::component::Element<'a>, String> {
        let state = self
            .states
            .get(block.id.as_str())
            .ok_or("missing prepared component state")?;
        self.factory.build(block, std::sync::Arc::clone(state))
    }
}
type LocalStates = std::collections::BTreeMap<
    bmux_presentation::custom::CompatibilityKey,
    bmux_presentation::custom::LinkedState,
>;
fn prepare_states(
    deck: &Deck,
    linked: &bmux_presentation::custom::LinkedRegistry,
    previous: &LocalStates,
) -> Result<LocalStates, String> {
    fn collect(
        blocks: &[bmux_presentation::Block],
        linked: &bmux_presentation::custom::LinkedRegistry,
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
            if let bmux_presentation::BlockKind::Columns { columns } = &block.kind {
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

pub(crate) struct Player {
    viewer: ViewerState,
    painted_viewer: ViewerState,
    pending_viewer: ViewerState,
    focused_block: Option<bmux_presentation::BlockId>,
    linked: bmux_presentation::custom::LinkedRegistry,
    instances: LocalStates,
    pub rich: crate::rich::RichAssets,
    pub surfaces: Vec<bmux_presentation_runtime::mirror::MirrorFrame>,
    painted_layout: Option<bmux_tui::component::LayoutNode>,
    pending_layout: Option<bmux_tui::component::LayoutNode>,
    mirror_geometry: Vec<(
        String,
        u64,
        String,
        bmux_presentation_runtime::mirror::CanvasPlacement,
    )>,
    pending_mirror_geometry: Vec<(
        String,
        u64,
        String,
        bmux_presentation_runtime::mirror::CanvasPlacement,
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
        }
    }
    fn registry(&self) -> CustomRegistry<'static> {
        let mut registry = CustomRegistry::new();
        if self.remote.is_some() {
            return registry;
        }
        let mut grouped = std::collections::BTreeMap::<
            String,
            std::collections::BTreeMap<String, bmux_presentation::custom::LinkedState>,
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
        let Some((surface_id, revision, slide_id, placement)) = self.mirror_geometry.first() else {
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
            Event::Key(key)
                if matches!(key.key, KeyCode::Char(_))
                    && !key.modifiers.ctrl
                    && !key.modifiers.alt =>
            {
                let KeyCode::Char(character) = key.key else {
                    unreachable!()
                };
                (0, 0, character.to_string())
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
        remote
            .actions
            .try_send(bmux_presentation_runtime::Action::DemoInput(
                bmux_presentation_runtime::SurfaceInput {
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
    fn focus_targets(&self, registry: &CustomRegistry<'_>) -> Vec<bmux_presentation::BlockId> {
        fn collect(
            blocks: &[bmux_presentation::Block],
            step: u32,
            registry: &CustomRegistry<'_>,
            out: &mut Vec<bmux_presentation::BlockId>,
        ) {
            for block in blocks.iter().filter(|block| block.reveal_step <= step) {
                match &block.kind {
                    bmux_presentation::BlockKind::Custom { name, .. }
                        if registry.contains(name) =>
                    {
                        out.push(block.id.clone())
                    }
                    bmux_presentation::BlockKind::Columns { columns } => {
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
                        && !targets.iter().any(|id: &bmux_presentation::BlockId| {
                            id.as_str() == surface.surface_id
                        })
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
    fn selected_surface(&self) -> Option<&bmux_presentation_runtime::mirror::MirrorFrame> {
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
    fn reload(&mut self) {
        if let Some(updates) = &mut self.reloads {
            for _ in 0..16 {
                let Ok(update) = updates.try_recv() else {
                    break;
                };
                match update {
                    Ok((deck, rich)) => {
                        let states = match prepare_states(&deck, &self.linked, &self.instances) {
                            Ok(states) => states,
                            Err(error) => {
                                self.status = Some(format!(
                                    "Reload rejected; retaining last valid deck: {error}"
                                ));
                                continue;
                            }
                        };
                        self.instances = states;
                        self.deck = deck;
                        self.rich = rich;
                        self.painted_layout = None;
                        self.mirror_geometry.clear();
                        self.viewer.reset_scroll();
                        reduce(&self.deck, &mut self.state, NavigationAction::Reconcile);
                        self.status = None;
                    }
                    Err(error) => {
                        self.status = Some(format!(
                            "Reload rejected; retaining last valid deck: {error}"
                        ))
                    }
                }
            }
        }
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
        self.painted_layout = self.pending_layout.take();
        self.painted_viewer
            .scroll
            .set(self.pending_viewer.scroll.get());
        self.mirror_geometry = std::mem::take(&mut self.pending_mirror_geometry);
        Update::none()
    }
    fn update(&mut self, event: RuntimeEvent<Message>) -> Result<Update<Message>> {
        match event {
            RuntimeEvent::Message(Message::Interrupted) => Ok(Update::exit()),
            RuntimeEvent::Message(Message::InputError(error)) => Err(error.into()),
            RuntimeEvent::Timer(_) => {
                self.reload();
                if let Some(remote) = &mut self.remote {
                    for _ in 0..16 {
                        let Ok(update) = remote.updates.try_recv() else {
                            break;
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
                                .try_send(bmux_presentation_runtime::Action::Navigate(action))
                                .context("navigation admission full or disconnected")?;
                        }
                    } else {
                        let previous_slide = self.state.slide_id.clone();
                        reduce(&self.deck, &mut self.state, action);
                        if previous_slide != self.state.slide_id {
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
                let _ = self.route_mirror(&event)?;
                let _ = self.route_component(&event);
                Ok(Update::redraw())
            }
        }
    }
}

fn clipped_mirror_placement(
    placement: bmux_presentation_runtime::mirror::CanvasPlacement,
    viewport: bmux_tui::component::LogicalRect,
    scroll_x: u64,
    scroll_y: u64,
) -> Option<bmux_presentation_runtime::mirror::CanvasPlacement> {
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
    Some(bmux_presentation_runtime::mirror::CanvasPlacement {
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

fn paint_content(player: &mut Player, theme: &PresentationTheme, cx: &mut PaintCx<'_, '_>) {
    let registry = player.registry();
    player.reconcile_focus(&registry);
    let area = cx.area();
    let notes_height = if player.presenter { area.height / 3 } else { 0 };
    let slide_height = area.height.saturating_sub(notes_height).saturating_sub(1);
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
    if let Some(id) = &player.focused_block {
        component = component.focused_block(id);
    }
    if let Some(mirror) = &mirror {
        component = component.content(mirror);
    }
    let layout = component.layout(
        Constraints::tight(Size::new(area.width, slide_height)),
        &mut LayoutCx::default(),
    );
    cx.with_child(
        0,
        0,
        LocalRect::new(0, 0, area.width, slide_height),
        |child| component.paint(&layout, child),
    );
    let mut pending_mirror_geometry = vec![];
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
    player.pending_mirror_geometry = pending_mirror_geometry;
    player.pending_viewer.scroll.set(player.viewer.scroll.get());
    player.pending_layout = Some(layout);
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

pub(crate) async fn run(player: Player) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal() && stdout().is_terminal(),
        "presentation playback requires stdin/stdout TTY"
    );
    let mut guard = TerminalGuard::enter()?;
    let result = run_owned(player).await;
    let restored = guard.restore();
    match (result, restored) {
        (_, Err(error)) => Err(error.context("presentation terminal restoration failed")),
        (result, Ok(())) => result,
    }
}

async fn run_owned(mut player: Player) -> Result<()> {
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
    let (cols, rows) = crossterm::terminal::size()?;
    let terminal = Terminal::new(stdout(), Rect::new(0, 0, cols, rows));
    let presenter =
        ImageTerminalPresenter::detect(terminal, paint, bmux_image::ImageConfig::default());
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
        Ok(mut output) => output
            .presenter
            .cleanup_images()
            .map_err(anyhow::Error::from),
        Err(bmux_tui_runtime::RuntimeError::Program { error, mut output }) => {
            output
                .presenter
                .cleanup_images()
                .context("image cleanup after program error")?;
            Err(error)
        }
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, mut output }) => {
            output
                .presenter
                .cleanup_images()
                .context("image cleanup after presentation error")?;
            Err(anyhow::anyhow!(error))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_keyboard::KeyStroke;
    use bmux_presentation::builder::{DeckBuilder, SlideBuilder};
    #[test]
    fn mirror_input_requires_committed_geometry_and_matching_revision() {
        use bmux_presentation_runtime::mirror::{MirrorFrame, WireCell};
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
        let bmux_presentation_runtime::Action::DemoInput(input) = actions.try_recv().unwrap()
        else {
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
        use bmux_presentation_runtime::mirror::CanvasPlacement;
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
