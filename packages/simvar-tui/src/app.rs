//! The dashboard's state and behaviour: what is open, what the pointer is over, and what each
//! click, key and simulation event does. Drawing lives in [`crate::views`].

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::geometry::Point;
use bmux_tui::hit::HitMap;
use bmux_tui::interaction::InteractionRouter;
use bmux_tui::paint::PaintCx;
use simvar_observer::{Breakpoint, RunId, RunKind, SimEvent};

use crate::findings::{Finding, Findings, build_identity};
use crate::store::{Notice, RunRecord, Store};
use crate::telemetry::{Interest, RunIndex, Tone};
use crate::theme::Theme;
use crate::timetravel::TimeTravel;
use crate::ui::Areas;
use crate::{Controller, Options, SimView};

/// How long a toast stays.
const TOAST: Duration = Duration::from_secs(6);
/// Rows a wheel notch scrolls.
const WHEEL: u16 = 3;

/// A page of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Threads,
    Invariants,
    Decisions,
    View(usize),
}

impl Page {
    pub const fn id(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Threads => 1,
            Self::Invariants => 2,
            Self::Decisions => 3,
            Self::View(index) => 4 + index,
        }
    }

    pub const fn from_id(id: usize) -> Self {
        match id {
            0 => Self::Overview,
            1 => Self::Threads,
            2 => Self::Invariants,
            3 => Self::Decisions,
            n => Self::View(n - 4),
        }
    }
}

/// An open run.
#[derive(Debug)]
pub struct RunTab {
    /// The run opened (a fleet run, or the failed run of a failure group).
    pub origin: RunId,
    pub seed: u64,
    pub epoch_offset: u64,
    pub label: String,
    pub page: Page,
    /// The entity opened in full, on a view page (or the thread, on Threads).
    pub entity: Option<String>,
    /// The entity selected (its card shows), on a view page.
    pub selected: Option<String>,
    /// Replays, once the run is being travelled through.
    pub travel: Option<TimeTravel>,
    /// The decision range the timeline shows, when zoomed.
    pub zoom: Option<(u64, u64)>,
}

impl RunTab {
    /// The run whose state is shown: the replay at the cursor, or the run itself.
    pub fn shown(&self) -> RunId {
        self.travel
            .as_ref()
            .and_then(|travel| travel.cursor.run)
            .unwrap_or(self.origin)
    }
}

/// A tab.
#[derive(Debug)]
pub enum Tab {
    Fleet,
    Failures,
    Run(Box<RunTab>),
}

/// Something to do, from a click, a key or the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Tab(usize),
    CloseTab(usize),
    Back,
    Crumb(usize),
    OpenRun(RunId),
    OpenFailure(usize),
    SelectFailure(usize),
    Confirm(usize),
    Shrink(usize),
    Page(Page),
    Select(String),
    Open(String),
    Lane(usize),
    Seek(u64),
    Step(i64),
    TogglePlay,
    PrevEvent,
    NextEvent,
    Start,
    End,
    ZoomOut,
    ToggleFleet,
    Parallel(i64),
    SetParallel(u64),
    Filter(Option<String>),
    Theme,
    Help,
    Palette,
    Quit,
    BreakDecision(u64),
    BreakEvent(String),
    BreakFailure,
    ClearBreaks,
    Dismiss(u64),
}

impl Action {
    /// The action a hit region's id stands for.
    pub fn from_hit(id: &str) -> Option<Self> {
        let mut parts = id.split('.');
        let head = parts.next()?;
        let number = |part: Option<&str>| part.and_then(|p| p.parse::<u64>().ok());
        let index = |part: Option<&str>| part.and_then(|p| p.parse::<usize>().ok());
        Some(match head {
            "tab" => {
                let tab = index(parts.next())?;
                if parts.next() == Some("close") {
                    Self::CloseTab(tab)
                } else {
                    Self::Tab(tab)
                }
            }
            "crumb" => Self::Crumb(index(parts.next())?),
            "worker" | "run" => {
                let worker = number(parts.next())?;
                let run = number(parts.next())?;
                Self::OpenRun(RunId { worker, run })
            }
            "failure" => {
                let group = index(parts.next())?;
                match parts.next() {
                    Some("open") => Self::OpenFailure(group),
                    Some("confirm") => Self::Confirm(group),
                    Some("shrink") => Self::Shrink(group),
                    _ => Self::SelectFailure(group),
                }
            }
            "recent" => Self::OpenFailure(index(parts.next())?),
            "page" => Self::Page(Page::from_id(index(parts.next())?)),
            "entity" => {
                let rest: Vec<&str> = parts.collect();
                Self::Select(rest.join("."))
            }
            "lane" => Self::Lane(index(parts.next())?),
            "event" => Self::Seek(number(parts.next())?),
            "transport" => match parts.next()? {
                "play" => Self::TogglePlay,
                "back" => Self::Step(-1),
                "forward" => Self::Step(1),
                "back100" => Self::Step(-100),
                "forward100" => Self::Step(100),
                "start" => Self::Start,
                "end" => Self::End,
                "previous" => Self::PrevEvent,
                "next" => Self::NextEvent,
                "zoom" => Self::ZoomOut,
                _ => return None,
            },
            "fleet" => match parts.next()? {
                "toggle" => Self::ToggleFleet,
                "more" => Self::Parallel(1),
                "fewer" => Self::Parallel(-1),
                _ => return None,
            },
            "filter" => {
                let rest: Vec<&str> = parts.collect();
                let tag = rest.join(".");
                Self::Filter((!tag.is_empty()).then_some(tag))
            }
            "theme" => Self::Theme,
            "help" => Self::Help,
            "palette" => {
                if parts.next() == Some("item") {
                    return None;
                }
                Self::Palette
            }
            "toast" => Self::Dismiss(number(parts.next())?),
            "break" => match parts.next()? {
                "failure" => Self::BreakFailure,
                "clear" => Self::ClearBreaks,
                _ => return None,
            },
            _ => return None,
        })
    }
}

/// A passing notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub tone: Tone,
    pub title: String,
    pub body: String,
    pub action: Option<Action>,
    pub born: Instant,
}

/// The command palette's state.
#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    pub query: String,
    pub selected: usize,
}

/// One palette entry.
#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub label: String,
    pub hint: String,
    pub action: Action,
}

/// Background jobs the dashboard starts on its own for failures.
#[derive(Debug, Default)]
struct Jobs {
    confirm: VecDeque<usize>,
    shrink: VecDeque<usize>,
    shrinking: Option<usize>,
}

/// The dashboard's state; see the [module documentation](self).
pub struct App {
    pub store: Store,
    pub views: Vec<Box<dyn SimView>>,
    pub controller: Option<Box<dyn Controller>>,
    interest: Arc<Interest>,
    findings: Option<Findings>,
    build: String,
    pub theme: Theme,
    pub title: String,
    pub tabs: Vec<Tab>,
    pub tab: usize,
    router: InteractionRouter,
    pub hover: Option<String>,
    pub areas: Areas,
    pub toasts: Vec<Toast>,
    next_toast: u64,
    pub palette: Option<PaletteState>,
    pub help: bool,
    pub scroll: BTreeMap<String, u16>,
    /// Where each draggable divider sits, in percent of its area.
    pub splits: BTreeMap<String, u16>,
    /// What the pointer is dragging (a divider or a seeking surface), from press to release.
    dragging: Option<String>,
    pub selected_failure: usize,
    pub selected_worker: usize,
    pub filter: Option<String>,
    pub message: Option<(String, Instant)>,
    jobs: Jobs,
    pub started: Instant,
    pub now: Instant,
    last_second: Instant,
    dirty: bool,
    quit: bool,
}

impl App {
    /// A dashboard's state, as `options` say.
    #[must_use]
    pub fn new(options: Options, now: Instant) -> Self {
        let mut store = Store::new(now);
        let build = build_identity();
        if let Some(findings) = &options.findings {
            for finding in findings.load() {
                store.add_saved(finding.group(&build, now));
            }
        }
        let mut app = Self {
            store,
            views: options.views,
            controller: options.controller,
            interest: options.interest,
            findings: options.findings,
            build,
            theme: Theme::dark(),
            title: options.title,
            tabs: vec![Tab::Fleet, Tab::Failures],
            tab: 0,
            router: InteractionRouter::new(),
            hover: None,
            areas: Areas::default(),
            toasts: Vec::new(),
            next_toast: 0,
            palette: None,
            help: false,
            scroll: BTreeMap::new(),
            splits: BTreeMap::new(),
            dragging: None,
            selected_failure: 0,
            selected_worker: 0,
            filter: None,
            message: None,
            jobs: Jobs::default(),
            started: now,
            now,
            last_second: now,
            dirty: true,
            quit: false,
        };
        if let Some((seed, epoch_offset)) = options.open {
            app.open_seed(seed, epoch_offset);
        }
        app
    }

    /// The store (for tests and embedding).
    #[must_use]
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Whether the user quit.
    #[must_use]
    pub const fn quitting(&self) -> bool {
        self.quit
    }

    /// Whether the screen is out of date (something changed, or something animates).
    #[must_use]
    pub fn needs_draw(&self) -> bool {
        self.dirty || self.animating()
    }

    fn animating(&self) -> bool {
        !self.toasts.is_empty()
            || self.jobs.shrinking.is_some()
            || self.store.failures.iter().any(|g| g.confirming.is_some())
            || self.tabs.iter().any(|tab| match tab {
                Tab::Run(run) => run
                    .travel
                    .as_ref()
                    .is_some_and(|t| t.playing.is_some() || !t.settled()),
                _ => false,
            })
            || (self.current_run().is_some_and(|run| run.travel.is_none())
                && self.store.runs.values().any(RunRecord::live))
    }

    /// Fold one simulation event.
    pub fn event(&mut self, event: SimEvent, now: Instant) {
        if let SimEvent::Telemetry { id, payload } = &event {
            for view in &mut self.views {
                view.telemetry(*id, payload);
            }
        }
        for notice in self.store.apply(event, now) {
            self.notice(notice, now);
        }
        self.dirty = true;
    }

    fn notice(&mut self, notice: Notice, now: Instant) {
        match notice {
            Notice::NewFailure(group) => {
                let signature = self.store.failures[group].signature.clone();
                self.toast(
                    Tone::Bad,
                    "New failure",
                    signature,
                    Some(Action::OpenFailure(group)),
                    now,
                );
                self.jobs.confirm.push_back(group);
                self.save(group);
            }
            Notice::RepeatFailure(group) => self.save(group),
            Notice::Confirmed(group, reproduced) => {
                let signature = self.store.failures[group].signature.clone();
                if reproduced {
                    self.toast(
                        Tone::Info,
                        "Failure reproduced",
                        signature,
                        Some(Action::OpenFailure(group)),
                        now,
                    );
                    self.jobs.shrink.push_back(group);
                } else {
                    self.toast(
                        Tone::Warn,
                        "Failure did not reproduce",
                        format!("{signature} (nondeterminism?)"),
                        Some(Action::OpenFailure(group)),
                        now,
                    );
                }
                self.save(group);
            }
            Notice::Shrunk(group) => {
                if self.jobs.shrinking == Some(group) {
                    self.jobs.shrinking = None;
                }
                if let Some(shrink) = &self.store.failures[group].shrink {
                    let body = format!("{} → {} scheduling choices", shrink.original, shrink.best);
                    self.toast(
                        Tone::Good,
                        "Failure shrunk",
                        body,
                        Some(Action::OpenFailure(group)),
                        now,
                    );
                }
                self.save(group);
            }
            Notice::Finished => {
                self.toast(Tone::Info, "Simulation finished", String::new(), None, now);
            }
        }
    }

    fn save(&self, group: usize) {
        if let (Some(findings), Some(group)) = (&self.findings, self.store.failures.get(group)) {
            let _ = findings.save(&Finding::of(group, &self.build));
        }
    }

    pub fn toast(
        &mut self,
        tone: Tone,
        title: &str,
        body: String,
        action: Option<Action>,
        now: Instant,
    ) {
        self.next_toast += 1;
        self.toasts.push(Toast {
            id: self.next_toast,
            tone,
            title: title.into(),
            body,
            action,
            born: now,
        });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        self.dirty = true;
    }

    /// Advance time: throughput samples, playing replays, background jobs, expiring toasts,
    /// and what the simulation should send.
    pub fn tick(&mut self, now: Instant) {
        self.now = now;
        if now.duration_since(self.last_second) >= Duration::from_secs(1) {
            self.store.tick(now);
            self.last_second = now;
            self.dirty = true;
        }
        let before = self.toasts.len();
        self.toasts.retain(|t| now.duration_since(t.born) < TOAST);
        if self.toasts.len() != before {
            self.dirty = true;
        }
        if self
            .message
            .as_ref()
            .is_some_and(|(_, at)| now.duration_since(*at) > TOAST)
        {
            self.message = None;
            self.dirty = true;
        }
        self.travel_frames();
        self.pump_jobs();
        self.interest.set_watched(self.watched());
    }

    fn travel_frames(&mut self) {
        let Some(controller) = self.controller.as_deref() else {
            return;
        };
        for tab in &mut self.tabs {
            let Tab::Run(run) = tab else { continue };
            let length = self
                .store
                .runs
                .get(&run.origin)
                .and_then(|r| r.index.as_ref())
                .filter(|index| index.finished)
                .map(|index| index.decisions);
            if let Some(travel) = &mut run.travel {
                if let Some(length) = length {
                    travel.set_length(controller, length);
                }
                travel.frame(controller);
            }
        }
    }

    fn pump_jobs(&mut self) {
        let Some(controller) = self.controller.as_deref() else {
            return;
        };
        // One confirmation and one shrink at a time, so jobs never crowd out the fleet.
        if !self.store.failures.iter().any(|g| g.confirming.is_some())
            && let Some(group) = self.jobs.confirm.pop_front()
            && let Some(failure) = self.store.failures.get(group)
            && let Some(job) = controller.replay(failure.seed, failure.epoch_offset, None)
        {
            self.store.confirming(group, job);
            self.dirty = true;
        }
        if self.jobs.shrinking.is_none()
            && let Some(group) = self.jobs.shrink.pop_front()
            && let Some(failure) = self.store.failures.get(group)
            && let Some(job) = controller.shrink(failure.seed, failure.epoch_offset)
        {
            self.store.shrinking(group, job);
            self.jobs.shrinking = Some(group);
            self.dirty = true;
        }
    }

    /// The runs whose detailed telemetry is wanted: open runs and the replays shown.
    fn watched(&self) -> Vec<RunId> {
        self.tabs
            .iter()
            .filter_map(|tab| match tab {
                Tab::Run(run) => Some(run),
                _ => None,
            })
            .flat_map(|run| {
                std::iter::once(run.origin).chain(run.travel.as_ref().and_then(|t| t.cursor.run))
            })
            .collect()
    }

    /// Draw the dashboard.
    pub fn render(&mut self, cx: &mut PaintCx<'_, '_>) {
        self.areas.clear();
        crate::views::render(self, cx);
        self.dirty = false;
    }

    /// Take the hit regions of the frame just drawn.
    pub fn commit(&mut self, hits: &HitMap) {
        self.router.commit_scene(hits.clone(), None);
        let hover = self.router.hovered().map(|id| id.as_str().to_owned());
        if hover != self.hover {
            self.hover = hover;
            self.dirty = true;
        }
    }

    /// Release what the dashboard holds in the simulation (replays) when it closes.
    pub fn close(&self) {
        for tab in &self.tabs {
            if let Tab::Run(run) = tab
                && let Some(travel) = &run.travel
            {
                travel.close();
            }
        }
        self.interest.set_watched([]);
    }

    pub fn current_run(&self) -> Option<&RunTab> {
        match self.tabs.get(self.tab)? {
            Tab::Run(run) => Some(run),
            _ => None,
        }
    }

    fn current_run_mut(&mut self) -> Option<&mut RunTab> {
        match self.tabs.get_mut(self.tab)? {
            Tab::Run(run) => Some(run),
            _ => None,
        }
    }

    /// The decision the current run tab shows, and its length when known.
    #[allow(clippy::option_if_let_else)] // reads better as a match
    pub fn position(&self) -> Option<(u64, Option<u64>)> {
        let run = self.current_run()?;
        let index = self.index(run.origin);
        let length = index
            .as_ref()
            .filter(|index| index.finished)
            .map(|index| index.decisions);
        Some(match &run.travel {
            Some(travel) => (travel.position(), length.or(travel.length)),
            None => (
                self.store
                    .runs
                    .get(&run.origin)
                    .map_or(0, |r| r.decisions)
                    .saturating_sub(1),
                length,
            ),
        })
    }

    /// The timeline index of `run`.
    pub fn index(&self, run: RunId) -> Option<Arc<RunIndex>> {
        self.store.runs.get(&run).and_then(|r| r.index.clone())
    }

    /// Handle terminal input.
    pub fn input(&mut self, event: &Event, now: Instant) {
        self.now = now;
        self.dirty = true;
        match event {
            Event::Key(stroke) => {
                if let Some(action) = self.key(*stroke) {
                    self.act(action);
                }
            }
            Event::Mouse(mouse) => {
                let route = self.router.route(event.clone());
                self.hover = self.router.hovered().map(|id| id.as_str().to_owned());
                let mut target = route.target.as_ref().map(|id| id.as_str().to_owned());
                let draggable =
                    |id: &str| id == "timeline" || id == "scrub" || id.starts_with("split.");
                match mouse.kind {
                    MouseEventKind::Down(_) => {
                        self.dragging = target.clone().filter(|id| draggable(id));
                    }
                    MouseEventKind::Drag(_) => target.clone_from(&self.dragging),
                    MouseEventKind::Up(_) if self.dragging.take().is_some() => return,
                    _ => {}
                }
                match mouse.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let down = matches!(mouse.kind, MouseEventKind::ScrollDown);
                        if let Some(list) = self.list_at(mouse.position) {
                            self.scroll_list(&list, down);
                        } else if self.current_run().is_some()
                            && target.as_deref() == Some("timeline")
                        {
                            self.act(Action::Step(if down { 1 } else { -1 }));
                        }
                    }
                    MouseEventKind::Down(_) | MouseEventKind::Drag(_)
                        if matches!(target.as_deref(), Some("timeline" | "scrub")) =>
                    {
                        let surface = target.as_deref().unwrap_or("timeline");
                        if let Some(decision) = self.seek_decision(surface, mouse.position) {
                            self.act(Action::Seek(decision));
                        }
                    }
                    MouseEventKind::Down(_) | MouseEventKind::Drag(_)
                        if target.as_deref().is_some_and(|t| t.starts_with("split.")) =>
                    {
                        if let Some(id) = target {
                            self.drag_split(&id, mouse.position);
                        }
                    }
                    _ => {
                        if route.is_activation()
                            && let Some(action) = target.as_deref().and_then(Action::from_hit)
                        {
                            self.act(action);
                        } else if route.is_activation()
                            && let Some(item) = target
                                .as_deref()
                                .and_then(|t| t.strip_prefix("palette.item."))
                                .and_then(|i| i.parse::<usize>().ok())
                            && let Some(item) = self.palette_items().get(item).cloned()
                        {
                            self.palette = None;
                            self.act(item.action);
                        }
                    }
                }
            }
            Event::Paste(text) => {
                if let Some(palette) = &mut self.palette {
                    palette.query.push_str(text);
                }
            }
            _ => {}
        }
    }

    /// The scrollable list under `point`, from the last frame's hit regions (`list.<name>`).
    fn list_at(&self, point: Point) -> Option<String> {
        self.router
            .scene()
            .regions()
            .iter()
            .rev()
            .find(|region| region.area.contains(point) && region.id.as_str().starts_with("list."))
            .map(|region| region.id.as_str()["list.".len()..].to_owned())
    }

    fn scroll_list(&mut self, list: &str, down: bool) {
        let offset = self.scroll.entry(list.to_owned()).or_default();
        *offset = if down {
            offset.saturating_add(WHEEL)
        } else {
            offset.saturating_sub(WHEEL)
        };
    }

    /// Move the divider `id` (`split.col.…` or `split.row.…`) to `point`.
    fn drag_split(&mut self, id: &str, point: Point) {
        let Some(area) = self.areas.get(id) else {
            return;
        };
        let name = &id["split.".len()..];
        let percent = if name.starts_with("col.") {
            u32::from(point.x.saturating_sub(area.x)) * 100 / u32::from(area.width.max(1))
        } else {
            u32::from(point.y.saturating_sub(area.y)) * 100 / u32::from(area.height.max(1))
        };
        self.splits.insert(
            name.to_owned(),
            u16::try_from(percent.clamp(10, 90)).unwrap_or(50),
        );
    }

    /// The decision under `point` on a seeking surface drawn last frame: the timeline (its
    /// shown range) or the scrubber (the whole run).
    fn seek_decision(&self, surface: &str, point: Point) -> Option<u64> {
        let area = self.areas.get(surface)?;
        let (lo, hi) = if surface == "scrub" {
            let (position, length) = self.position()?;
            (0, length.unwrap_or(position + 1).max(1))
        } else {
            self.timeline_range()?
        };
        let x = u64::from(
            point
                .x
                .saturating_sub(area.x)
                .min(area.width.saturating_sub(1)),
        );
        let width = u64::from(area.width.max(1));
        Some(lo + (hi - lo) * x / width)
    }

    /// The decisions the timeline spans.
    pub fn timeline_range(&self) -> Option<(u64, u64)> {
        let run = self.current_run()?;
        if let Some(zoom) = run.zoom {
            return Some(zoom);
        }
        let (position, length) = self.position()?;
        let end = length
            .or_else(|| self.index(run.origin).map(|i| i.decisions))
            .unwrap_or(position + 1)
            .max(position + 1)
            .max(1);
        Some((0, end))
    }

    fn key(&mut self, stroke: KeyStroke) -> Option<Action> {
        let modifiers = stroke.modifiers;
        let code = match stroke.key {
            KeyCode::Char(c) if modifiers.shift && c.is_ascii_lowercase() => {
                KeyCode::Char(c.to_ascii_uppercase())
            }
            other => other,
        };
        if modifiers.ctrl {
            return match code {
                KeyCode::Char('c') => Some(Action::Quit),
                KeyCode::Char('k') => Some(Action::Palette),
                _ => None,
            };
        }
        if let Some(palette) = &mut self.palette {
            match code {
                KeyCode::Escape => self.palette = None,
                KeyCode::Enter => {
                    let query = palette.query.clone();
                    let selected = palette.selected;
                    self.palette = None;
                    if let Some(action) = parse_command(&query) {
                        return Some(action);
                    }
                    return self
                        .palette_items_for(&query)
                        .get(selected)
                        .map(|item| item.action.clone());
                }
                KeyCode::Backspace => {
                    palette.query.pop();
                    palette.selected = 0;
                }
                KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
                KeyCode::Down => palette.selected += 1,
                KeyCode::Char(c) => {
                    palette.query.push(c);
                    palette.selected = 0;
                }
                _ => {}
            }
            return None;
        }
        if self.help {
            self.help = false;
            return None;
        }
        let in_run = self.current_run().is_some();
        let fast = if modifiers.shift { 100 } else { 1 };
        Some(match code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('?') => Action::Help,
            KeyCode::Char(':') => Action::Palette,
            KeyCode::Char('t') => Action::Theme,
            KeyCode::Char('p') => Action::ToggleFleet,
            KeyCode::Char('+' | '=') => Action::Parallel(1),
            KeyCode::Char('-') => Action::Parallel(-1),
            KeyCode::Char('f') => Action::Tab(1),
            KeyCode::Tab => Action::Tab(
                (self.tab
                    + if modifiers.shift {
                        self.tabs.len() - 1
                    } else {
                        1
                    })
                    % self.tabs.len(),
            ),
            KeyCode::Escape => Action::Back,
            KeyCode::Char(c @ '1'..='9') => Action::Tab(c as usize - '1' as usize),
            KeyCode::Char(' ') if in_run => Action::TogglePlay,
            KeyCode::Left if in_run => Action::Step(-fast),
            KeyCode::Right if in_run => Action::Step(fast),
            KeyCode::Char('H') if in_run => Action::Step(-100),
            KeyCode::Char('L') if in_run => Action::Step(100),
            KeyCode::Char('h') if in_run => Action::Step(-1),
            KeyCode::Char('l') if in_run => Action::Step(1),
            KeyCode::Char('[') if in_run => Action::PrevEvent,
            KeyCode::Char(']') if in_run => Action::NextEvent,
            KeyCode::Home if in_run => Action::Start,
            KeyCode::End if in_run => Action::End,
            KeyCode::Char('z') if in_run => Action::ZoomOut,
            KeyCode::Char('b') if in_run => {
                Action::BreakDecision(self.position().map_or(0, |(p, _)| p + 1))
            }
            KeyCode::Char('<') if in_run => self.page_step(-1)?,
            KeyCode::Char('>') if in_run => self.page_step(1)?,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                return None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                return None;
            }
            KeyCode::Enter => self.enter()?,
            KeyCode::Char('c') if self.tab == 1 => Action::Confirm(self.selected_failure),
            KeyCode::Char('s') if self.tab == 1 => Action::Shrink(self.selected_failure),
            _ => return None,
        })
    }

    fn page_step(&self, by: isize) -> Option<Action> {
        let run = self.current_run()?;
        let pages = 4 + self.views.len();
        let next = (run.page.id().cast_signed() + by).rem_euclid(pages.cast_signed());
        Some(Action::Page(Page::from_id(next.cast_unsigned())))
    }

    fn move_selection(&mut self, by: isize) {
        let step = |value: usize, len: usize| -> usize {
            if len == 0 {
                0
            } else {
                (value.cast_signed() + by)
                    .clamp(0, len.cast_signed() - 1)
                    .cast_unsigned()
            }
        };
        match self.tabs.get(self.tab) {
            Some(Tab::Fleet) => {
                let len = self.visible_workers().len();
                self.selected_worker = step(self.selected_worker, len);
            }
            Some(Tab::Failures) => {
                self.selected_failure = step(self.selected_failure, self.store.failures.len());
            }
            Some(Tab::Run(run)) => {
                if let Page::View(view) = run.page
                    && let Some(view) = self.views.get(view)
                {
                    let entities = view.entities(run.shown());
                    let at = run
                        .selected
                        .as_ref()
                        .and_then(|id| entities.iter().position(|e| &e.id == id))
                        .unwrap_or(0);
                    let next = step(at, entities.len());
                    let id = entities.get(next).map(|e| e.id.clone());
                    if let Some(run) = self.current_run_mut() {
                        run.selected = id;
                    }
                }
            }
            None => {}
        }
    }

    fn enter(&self) -> Option<Action> {
        match self.tabs.get(self.tab)? {
            Tab::Fleet => self
                .visible_workers()
                .get(self.selected_worker)
                .map(|run| Action::OpenRun(run.id)),
            Tab::Failures => Some(Action::OpenFailure(self.selected_failure)),
            Tab::Run(run) => run.selected.clone().map(Action::Open),
        }
    }

    /// The fleet's workers with the run each is making (as filtered), by worker.
    pub fn visible_workers(&self) -> Vec<&RunRecord> {
        self.store
            .workers
            .values()
            .filter_map(|worker| self.store.runs.get(&worker.current?))
            .filter(|run| {
                self.filter
                    .as_ref()
                    .is_none_or(|tag| run.shape.iter().any(|t| t == tag))
            })
            .collect()
    }

    /// Do `action`.
    #[allow(clippy::too_many_lines)] // one dispatch over every action
    pub fn act(&mut self, action: Action) {
        self.dirty = true;
        match action {
            Action::Tab(tab) if tab < self.tabs.len() => self.tab = tab,
            Action::Tab(_) => {}
            Action::CloseTab(tab) => self.close_tab(tab),
            Action::Back => self.back(),
            Action::Crumb(depth) => {
                // 0: the tab's root page; 1: a run's page; 2: the entity.
                if let Some(run) = self.current_run_mut() {
                    if depth <= 1 {
                        run.entity = None;
                    }
                    if depth == 0 {
                        run.page = Page::Overview;
                    }
                } else if depth == 0 {
                    self.tab = 0;
                }
            }
            Action::OpenRun(id) => self.open_run(id),
            Action::OpenFailure(group) => self.open_failure(group),
            Action::SelectFailure(group) => {
                self.selected_failure = group;
                if !matches!(self.tabs.get(self.tab), Some(Tab::Failures)) {
                    self.tab = 1;
                }
            }
            Action::Confirm(group) => {
                if self.store.failures.get(group).is_some() {
                    self.jobs.confirm.push_back(group);
                }
            }
            Action::Shrink(group) => {
                if self.store.failures.get(group).is_some() {
                    self.jobs.shrink.push_back(group);
                }
            }
            Action::Page(page) => {
                if let Some(run) = self.current_run_mut() {
                    run.page = page;
                    run.entity = None;
                }
            }
            Action::Select(id) => {
                if let Some(run) = self.current_run_mut() {
                    if run.selected.as_ref() == Some(&id) {
                        run.entity = Some(id);
                    } else {
                        run.selected = Some(id);
                    }
                }
            }
            Action::Open(id) => {
                if let Some(run) = self.current_run_mut() {
                    run.selected = Some(id.clone());
                    run.entity = Some(id);
                }
            }
            Action::Lane(thread) => {
                if let Some(run) = self.current_run_mut() {
                    run.page = Page::Threads;
                    run.entity = Some(thread.to_string());
                }
            }
            Action::Seek(decision) => self.travel(|travel, controller| {
                travel.set_playing(None);
                travel.seek(controller, decision);
            }),
            Action::Step(by) => self.travel(|travel, controller| {
                travel.set_playing(None);
                travel.step(controller, by);
            }),
            Action::TogglePlay => self.travel(|travel, _| {
                travel.set_playing(if travel.playing.is_some() {
                    None
                } else {
                    Some(1)
                });
            }),
            Action::PrevEvent | Action::NextEvent => {
                let next = action == Action::NextEvent;
                if let Some((position, _)) = self.position()
                    && let Some(run) = self.current_run()
                    && let Some(index) = self.index(run.origin)
                {
                    let event = if next {
                        index.next_event(position, None)
                    } else {
                        index.previous_event(position, None)
                    };
                    if let Some(event) = event {
                        let decision = event.decision;
                        self.act(Action::Seek(decision));
                    }
                }
            }
            Action::Start => self.act(Action::Seek(0)),
            Action::End => {
                let end = self.position().and_then(|(_, l)| l).unwrap_or(u64::MAX);
                self.act(Action::Seek(end.saturating_sub(1)));
            }
            Action::ZoomOut => {
                if let Some(run) = self.current_run_mut() {
                    run.zoom = None;
                }
            }
            Action::ToggleFleet => {
                if let Some(controller) = &self.controller {
                    if controller.status().paused {
                        controller.resume();
                        self.say("fleet resumed");
                    } else {
                        controller.pause();
                        self.say("fleet paused: no new runs start");
                    }
                }
            }
            Action::Parallel(by) => {
                if let Some(controller) = &self.controller {
                    let now = controller.status().max_parallel;
                    let next = now.saturating_add_signed(by).max(1);
                    controller.set_max_parallel(next);
                    self.say(&format!("{next} runs at once"));
                }
            }
            Action::SetParallel(count) => {
                if let Some(controller) = &self.controller {
                    controller.set_max_parallel(count.max(1));
                    self.say(&format!("{} runs at once", count.max(1)));
                }
            }
            Action::Filter(tag) => {
                self.filter = if self.filter == tag { None } else { tag };
                self.selected_worker = 0;
            }
            Action::Theme => self.theme = self.theme.toggled(),
            Action::Help => self.help = !self.help,
            Action::Palette => self.palette = Some(PaletteState::default()),
            Action::Quit => self.quit = true,
            Action::BreakDecision(decision) => self.break_at(Breakpoint::Decision(decision)),
            Action::BreakFailure => self.break_at(Breakpoint::Failure),
            Action::BreakEvent(kind) => {
                let on = !self.interest.breaks_on(&kind);
                self.interest.set_break(&kind, on);
                self.say(&format!(
                    "{} at `{kind}` events",
                    if on { "pausing" } else { "not pausing" }
                ));
            }
            Action::ClearBreaks => {
                if let Some(travel) = self.current_run().and_then(|r| r.travel.as_ref()) {
                    travel.cursor.control.clear_breakpoints();
                }
                for kind in self.interest.event_breaks() {
                    self.interest.set_break(&kind, false);
                }
                self.say("breakpoints cleared");
            }
            Action::Dismiss(id) => {
                if let Some(index) = self.toasts.iter().position(|t| t.id == id) {
                    let toast = self.toasts.remove(index);
                    if let Some(action) = toast.action {
                        self.act(action);
                    }
                }
            }
        }
    }

    fn say(&mut self, message: &str) {
        self.message = Some((message.to_owned(), self.now));
    }

    fn break_at(&mut self, breakpoint: Breakpoint) {
        let Some(run) = self.current_run() else {
            return;
        };
        if run.travel.is_none() {
            self.start_travel();
        }
        if let Some(travel) = self.current_run().and_then(|r| r.travel.as_ref()) {
            let text = format!("{breakpoint:?}");
            travel.cursor.control.add_breakpoint(breakpoint);
            self.say(&format!("breakpoint: {text}"));
        }
    }

    /// Run `f` on the current run's time travel, starting it first if the run is live.
    fn travel(&mut self, f: impl FnOnce(&mut TimeTravel, &dyn Controller)) {
        if self.current_run().is_some_and(|run| run.travel.is_none()) {
            self.start_travel();
        }
        let Some(controller) = self.controller.as_deref() else {
            return;
        };
        let Some(Tab::Run(run)) = self.tabs.get_mut(self.tab) else {
            return;
        };
        if let Some(travel) = &mut run.travel {
            f(travel, controller);
        }
    }

    /// Start replaying the current run from where it is shown.
    fn start_travel(&mut self) {
        let Some(controller) = self.controller.as_deref() else {
            self.message = Some((
                "this simulation cannot be replayed from the dashboard".into(),
                self.now,
            ));
            return;
        };
        let (position, length) = self.position().unwrap_or((0, None));
        let Some(Tab::Run(run)) = self.tabs.get_mut(self.tab) else {
            return;
        };
        run.travel = Some(TimeTravel::start(
            controller,
            run.seed,
            run.epoch_offset,
            position,
            length,
        ));
    }

    fn open_run(&mut self, id: RunId) {
        if let Some(index) = self.tabs.iter().position(|tab| match tab {
            Tab::Run(run) => run.origin == id,
            _ => false,
        }) {
            self.tab = index;
            return;
        }
        let Some(record) = self.store.runs.get(&id) else {
            return;
        };
        let label = format!(
            "{} #{}",
            if record.kind == RunKind::Fleet {
                format!("w{}", record.worker)
            } else {
                "job".into()
            },
            id.run
        );
        let tab = RunTab {
            origin: id,
            seed: record.seed,
            epoch_offset: record.epoch_offset,
            label,
            page: Page::Overview,
            entity: None,
            selected: None,
            travel: None,
            zoom: None,
        };
        let finished = !record.live();
        self.store.pin(id, true);
        self.tabs.push(Tab::Run(Box::new(tab)));
        self.tab = self.tabs.len() - 1;
        if finished {
            // A finished run is shown by replaying it, at its end.
            self.start_travel();
            self.act(Action::End);
        }
    }

    fn open_failure(&mut self, group: usize) {
        let Some(failure) = self.store.failures.get(group) else {
            return;
        };
        if let Some(run) = failure.runs.last().copied()
            && self.store.runs.contains_key(&run)
        {
            self.open_run(run);
            return;
        }
        let (seed, epoch_offset) = (failure.seed, failure.epoch_offset);
        self.open_seed(seed, epoch_offset);
    }

    /// Open the run of `seed` and `epoch_offset`, which this dashboard did not see run: it is
    /// replayed from its start.
    fn open_seed(&mut self, seed: u64, epoch_offset: u64) {
        let tab = RunTab {
            origin: RunId {
                worker: u64::MAX,
                run: seed,
            },
            seed,
            epoch_offset,
            label: format!("seed {}", crate::views::short_seed(seed)),
            page: Page::Overview,
            entity: None,
            selected: None,
            travel: None,
            zoom: None,
        };
        self.tabs.push(Tab::Run(Box::new(tab)));
        self.tab = self.tabs.len() - 1;
        self.start_travel();
        if let Some(Tab::Run(run)) = self.tabs.get_mut(self.tab)
            && let Some(travel) = &run.travel
            && let Some(replay) = travel.cursor.run
        {
            // The first replay stands in for the run itself.
            run.origin = replay;
        }
    }

    fn close_tab(&mut self, tab: usize) {
        if tab < 2 || tab >= self.tabs.len() {
            return;
        }
        if let Tab::Run(run) = self.tabs.remove(tab) {
            self.store.pin(run.origin, false);
            if let Some(travel) = &run.travel {
                travel.close();
            }
            for view in &mut self.views {
                view.forget(run.origin);
            }
        }
        if self.tab >= tab {
            self.tab = self.tab.saturating_sub(1).min(self.tabs.len() - 1);
        }
    }

    fn back(&mut self) {
        if self.palette.is_some() {
            self.palette = None;
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        let tab = self.tab;
        match self.tabs.get_mut(tab) {
            Some(Tab::Run(run)) => {
                if run.entity.is_some() {
                    run.entity = None;
                } else if run.page != Page::Overview {
                    run.page = Page::Overview;
                } else {
                    self.close_tab(tab);
                    self.tab = 0;
                }
            }
            Some(Tab::Failures) => self.tab = 0,
            _ => {}
        }
    }

    /// The palette's entries for its current query.
    pub fn palette_items(&self) -> Vec<PaletteItem> {
        let query = self
            .palette
            .as_ref()
            .map(|p| p.query.clone())
            .unwrap_or_default();
        self.palette_items_for(&query)
    }

    fn palette_items_for(&self, query: &str) -> Vec<PaletteItem> {
        let mut items = vec![
            item("Pause or resume the fleet", "p", Action::ToggleFleet),
            item("More runs at once", "+", Action::Parallel(1)),
            item("Fewer runs at once", "-", Action::Parallel(-1)),
            item("Fleet", "1", Action::Tab(0)),
            item("Failures", "f", Action::Tab(1)),
            item("Switch theme", "t", Action::Theme),
            item("Keys", "?", Action::Help),
            item("Quit", "q", Action::Quit),
        ];
        if self.current_run().is_some() {
            items.extend([
                item("Play or pause", "space", Action::TogglePlay),
                item("Start of the run", "home", Action::Start),
                item("End of the run", "end", Action::End),
                item(
                    "Break here (next decision)",
                    "b",
                    Action::BreakDecision(self.position().map_or(0, |(p, _)| p + 1)),
                ),
                item("Break on failures", "", Action::BreakFailure),
                item("Clear breakpoints", "", Action::ClearBreaks),
                item("Overview", "", Action::Page(Page::Overview)),
                item("Threads", "", Action::Page(Page::Threads)),
                item("Invariants", "", Action::Page(Page::Invariants)),
                item("Decisions", "", Action::Page(Page::Decisions)),
            ]);
            for (index, view) in self.views.iter().enumerate() {
                items.push(item(&view.title(), "", Action::Page(Page::View(index))));
            }
            if let Some(run) = self.current_run()
                && let Some(index) = self.index(run.origin)
            {
                let mut kinds: Vec<&str> = index.events.iter().map(|e| e.kind.as_str()).collect();
                kinds.sort_unstable();
                kinds.dedup();
                for kind in kinds {
                    items.push(item(
                        &format!("Pause at `{kind}` events"),
                        "",
                        Action::BreakEvent(kind.to_owned()),
                    ));
                }
            }
        }
        for run in self.store.live_fleet() {
            items.push(item(
                &format!(
                    "Open w{} run {} (seed {})",
                    run.worker,
                    run.id.run,
                    crate::views::short_seed(run.seed)
                ),
                "",
                Action::OpenRun(run.id),
            ));
        }
        for (index, group) in self.store.failures.iter().enumerate() {
            items.push(item(
                &format!("Failure: {}", group.signature),
                &format!("×{}", group.count),
                Action::OpenFailure(index),
            ));
        }
        let query = query.to_lowercase();
        if query.is_empty() {
            return items;
        }
        let mut scored: Vec<(usize, PaletteItem)> = items
            .into_iter()
            .filter_map(|item| fuzzy(&query, &item.label.to_lowercase()).map(|score| (score, item)))
            .collect();
        scored.sort_by_key(|(score, _)| *score);
        scored.into_iter().map(|(_, item)| item).collect()
    }
}

fn item(label: &str, hint: &str, action: Action) -> PaletteItem {
    PaletteItem {
        label: label.to_owned(),
        hint: hint.to_owned(),
        action,
    }
}

/// How well `query` matches `text` as a subsequence (lower is better), if at all.
fn fuzzy(query: &str, text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let mut first = None;
    let mut last = 0;
    for wanted in query.chars().filter(|c| !c.is_whitespace()) {
        let (at, _) = chars.find(|(_, c)| *c == wanted)?;
        first.get_or_insert(at);
        last = at;
    }
    Some(last - first.unwrap_or(0) + first.unwrap_or(0))
}

/// A typed palette command: `goto N`, `back N`, `forward N`, `break N`, `break fail`,
/// `break EVENT`, `parallel N`, `open SEED`.
fn parse_command(query: &str) -> Option<Action> {
    let mut words = query.split_whitespace();
    let verb = words.next()?;
    let argument = words.next();
    let number = argument.and_then(|a| a.replace(['_', ','], "").parse::<u64>().ok());
    Some(match (verb, argument) {
        ("goto" | "g", _) => Action::Seek(number?),
        ("back" | "b", _) => Action::Step(-i64::try_from(number.unwrap_or(1)).ok()?),
        ("forward" | "f", _) => Action::Step(i64::try_from(number.unwrap_or(1)).ok()?),
        ("break", Some("fail" | "failure" | "failures")) => Action::BreakFailure,
        ("break", Some("clear")) => Action::ClearBreaks,
        ("break", Some(kind)) => number.map_or_else(
            || Action::BreakEvent(kind.to_owned()),
            Action::BreakDecision,
        ),
        ("parallel" | "workers", _) => Action::SetParallel(number?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
