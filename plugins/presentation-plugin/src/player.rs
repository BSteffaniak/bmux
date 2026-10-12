//! Caller-local native playback; scheduling, input admission and image presentation
//! are exclusively delegated to bmux_tui_runtime.
use anyhow::{Context, Result, ensure};
use bmux_keyboard::KeyCode;
use bmux_presentation::component::{SlideComponent, SpeakerNotesComponent};
use bmux_presentation::custom::CustomRegistry;
use bmux_presentation::theme::PresentationTheme;
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

pub(crate) struct Player {
    counter: std::sync::Arc<std::sync::Mutex<i64>>,
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
        Self {
            counter: std::sync::Arc::new(std::sync::Mutex::new(0)),
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
            status: None,
        }
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
            Event::Key(key) if key.key == KeyCode::Char('+') => (0, 0, "+".to_owned()),
            Event::Key(key) if key.key == KeyCode::Char('-') => (0, 0, "-".to_owned()),
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
    fn route_component(&self, event: &Event) -> bmux_tui::event::EventOutcome {
        let (Some(layout), Some(slide)) =
            (&self.painted_layout, self.state.current_slide(&self.deck))
        else {
            return bmux_tui::event::EventOutcome::Ignored;
        };
        let theme = PresentationTheme::default();
        let registry = if self.remote.is_none() {
            crate::demo::registry(&self.counter)
        } else {
            CustomRegistry::new()
        };
        let mut component = SlideComponent::new(slide, self.state.reveal_step, &theme, &registry)
            .rich_content(&self.rich);
        if let Some(block) = slide.blocks.iter().find(|block| block.reveal_step <= self.state.reveal_step && matches!(&block.kind, bmux_presentation::BlockKind::Custom { name, .. } if name == "counter")) { component = component.focused_block(&block.id); }
        component.event(
            event,
            layout,
            &mut bmux_tui::component::EventCx::new(layout),
        )
    }
    fn reload(&mut self) {
        if let Some(updates) = &mut self.reloads {
            while let Ok(update) = updates.try_recv() {
                match update {
                    Ok((deck, rich)) => {
                        self.deck = deck;
                        self.rich = rich;
                        self.painted_layout = None;
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
                    while let Ok(update) = remote.updates.try_recv() {
                        match update {
                            Ok(update) => {
                                let snapshot = update.snapshot;
                                self.deck = snapshot.deck;
                                self.state = snapshot.state;
                                self.rich = update.rich;
                                self.surfaces = snapshot.surfaces;
                                self.painted_layout = None;
                                self.notes = if self.presenter { snapshot.notes } else { None };
                                self.status = update.acknowledgment;
                            }
                            Err(error) => return Err(anyhow::anyhow!(error)),
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
                        reduce(&self.deck, &mut self.state, action);
                        self.painted_layout = None;
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

fn paint(player: &mut Player, cx: &mut PaintCx<'_, '_>) {
    let theme = PresentationTheme::default();
    let registry = if player.remote.is_none() {
        crate::demo::registry(&player.counter)
    } else {
        CustomRegistry::new()
    };
    let area = cx.area();
    let Some(slide) = player.state.current_slide(&player.deck) else {
        return;
    };
    let notes_height = if player.presenter { area.height / 3 } else { 0 };
    let slide_height = area.height.saturating_sub(notes_height).saturating_sub(1);
    let mut component = SlideComponent::new(slide, player.state.reveal_step, &theme, &registry)
        .rich_content(&player.rich);
    if let Some(block) = slide.blocks.iter().find(|block| block.reveal_step <= player.state.reveal_step && matches!(&block.kind, bmux_presentation::BlockKind::Custom { name, .. } if name == "counter")) { component = component.focused_block(&block.id); }
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
    player.pending_layout = Some(layout);
    player.pending_mirror_geometry.clear();
    // Only the topmost complete fixed scene owns input; overlapping scenes are
    // not broadcast to every surface. Host currently publishes a complete canvas.
    for surface in player
        .surfaces
        .iter()
        .rev()
        .filter(|surface| surface.slide_id == slide.id.as_str())
        .take(1)
    {
        if surface.slide_id == slide.id.as_str() {
            let Ok(mirror) = crate::mirror::MirrorComponent::new(surface, 16_384, 512 * 1024)
            else {
                player.status = Some("invalid retained mirror scene".into());
                continue;
            };
            let layout = mirror.layout(
                Constraints::tight(Size::new(area.width, slide_height)),
                &mut LayoutCx::default(),
            );
            cx.with_child(
                0,
                0,
                LocalRect::new(0, 0, area.width, slide_height),
                |child| mirror.paint(&layout, child),
            );
            player.pending_mirror_geometry.push((
                surface.surface_id.clone(),
                surface.revision,
                surface.slide_id.clone(),
                mirror
                    .placement(&layout)
                    .expect("mirror produced its own layout"),
            ));
        }
    }
    if player.presenter {
        // Local notes only. Remote notes are supplied by an authorized Host projection.
        let mut projected = slide.clone();
        if let Some(notes) = &player.notes {
            projected.notes.text.clone_from(notes);
        }
        let notes = SpeakerNotesComponent {
            slide: &projected,
            theme: &theme,
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
            "{}  reveal {}  ←/→ navigate · q quit{}",
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
