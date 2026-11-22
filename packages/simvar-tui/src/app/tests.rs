use std::sync::Mutex;

use bmux_keyboard::Modifiers;
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{MouseButton, MouseEvent};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use simvar_observer::ReplayControl;

use super::*;
use crate::telemetry::{InvariantStatus, Invariants, Pulse, Shape, TimelineEvent};
use crate::{Entity, FleetState};

const WIDTH: u16 = 140;
const HEIGHT: u16 = 40;

#[derive(Default)]
struct Calls {
    replays: Vec<(u64, bool)>,
    shrinks: Vec<u64>,
    paused: bool,
    parallel: u64,
    controls: Vec<Arc<ReplayControl>>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Calls>>);

impl Controller for Fake {
    fn status(&self) -> FleetState {
        let calls = self.0.lock().unwrap();
        FleetState {
            paused: calls.paused,
            max_parallel: calls.parallel.max(2),
            active: 2,
        }
    }
    fn pause(&self) {
        self.0.lock().unwrap().paused = true;
    }
    fn resume(&self) {
        self.0.lock().unwrap().paused = false;
    }
    fn set_max_parallel(&self, count: u64) {
        self.0.lock().unwrap().parallel = count;
    }
    fn stop(&self) {}
    fn replay(&self, seed: u64, _: u64, control: Option<Arc<ReplayControl>>) -> Option<RunId> {
        let mut calls = self.0.lock().unwrap();
        calls.replays.push((seed, control.is_some()));
        if let Some(control) = control {
            calls.controls.push(control);
        }
        Some(RunId {
            worker: 0,
            run: u64::try_from(calls.replays.len()).unwrap(),
        })
    }
    fn shrink(&self, seed: u64, _: u64) -> Option<RunId> {
        self.0.lock().unwrap().shrinks.push(seed);
        Some(RunId {
            worker: 0,
            run: 1000,
        })
    }
}

/// A view listing two processes.
struct Processes;

impl SimView for Processes {
    fn title(&self) -> String {
        "Kernel".into()
    }
    fn telemetry(&mut self, _: RunId, _: &Arc<dyn std::any::Any + Send + Sync>) {}
    fn entities(&self, _: RunId) -> Vec<Entity> {
        ["sh", "sleep"]
            .iter()
            .enumerate()
            .map(|(i, name)| Entity {
                id: format!("p{i}"),
                kind: "process".into(),
                label: (*name).into(),
                status: "running".into(),
                tone: Tone::Good,
                depth: u16::try_from(i).unwrap(),
            })
            .collect()
    }
    fn card(&self, _: RunId, entity: &str, _: u16) -> Vec<bmux_tui::text::Line> {
        vec![bmux_tui::text::Line::raw(format!("card of {entity}"))]
    }
}

fn app() -> (App, Fake) {
    let fake = Fake::default();
    let app = App::new(
        Options {
            title: "pane-runtime".into(),
            views: vec![Box::new(Processes)],
            controller: Some(Box::new(fake.clone())),
            interest: Arc::new(Interest::default()),
            findings: None,
            open: None,
        },
        Instant::now(),
    );
    (app, fake)
}

fn id(worker: u64, run: u64) -> RunId {
    RunId { worker, run }
}

fn start(app: &mut App, run: RunId, seed: u64) {
    app.event(
        SimEvent::RunStarted {
            id: run,
            seed,
            props: vec![],
            steps: Some(100),
            worker: run.worker,
            kind: RunKind::Fleet,
            epoch_offset: 3,
        },
        Instant::now(),
    );
    app.event(
        SimEvent::Telemetry {
            id: run,
            payload: Arc::new(Shape(vec!["personality:windows".into()])),
        },
        Instant::now(),
    );
}

fn finish(app: &mut App, run: RunId, seed: u64, success: bool) {
    let mut index = RunIndex::default();
    for n in 0..400 {
        index.record(n, usize::try_from(n % 3).unwrap(), || {
            format!("thread-{}", n % 3)
        });
    }
    index.event(TimelineEvent {
        decision: 120,
        kind: "crash".into(),
        label: "server crashed".into(),
        tone: Tone::Warn,
        entity: None,
    });
    index.finished = true;
    app.event(
        SimEvent::Telemetry {
            id: run,
            payload: Arc::new(index),
        },
        Instant::now(),
    );
    app.event(
        SimEvent::Telemetry {
            id: run,
            payload: Arc::new(Invariants(vec![InvariantStatus {
                name: "6. crash sweep".into(),
                checked: 4,
                failures: vec![],
            }])),
        },
        Instant::now(),
    );
    app.event(
        SimEvent::RunFinished {
            id: run,
            seed,
            success,
            report: if success {
                String::new()
            } else {
                "Error:\n6. the crashed server's sessions still run [630]\n".into()
            },
            steps: 100,
            real_time: Duration::from_millis(5),
            sim_time: Duration::from_secs(2),
        },
        Instant::now(),
    );
}

/// Draw a frame, commit its hit regions, and return its text.
fn draw(app: &mut App) -> String {
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    app.render(&mut PaintCx::new(&mut frame));
    let hits = frame.hits().clone();
    drop(frame);
    app.commit(&hits);
    crate::views::frame_text(&buffer, HEIGHT)
}

fn region(app: &App, id: &str) -> Rect {
    app.router
        .scene()
        .regions()
        .iter()
        .find(|r| r.id.as_str() == id)
        .unwrap_or_else(|| panic!("no region {id}"))
        .area
}

fn click(app: &mut App, id: &str) {
    let area = region(app, id);
    let at = bmux_tui::geometry::Point::new(area.x, area.y);
    for kind in [
        MouseEventKind::Move,
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.input(&Event::Mouse(MouseEvent::new(kind, at)), Instant::now());
    }
}

fn key(app: &mut App, code: KeyCode) {
    app.input(
        &Event::Key(KeyStroke {
            key: code,
            modifiers: Modifiers::NONE,
        }),
        Instant::now(),
    );
}

#[test]
fn the_fleet_shows_workers_outcomes_and_invariants() {
    let (mut app, _) = app();
    start(&mut app, id(1, 1), 0xabc);
    start(&mut app, id(2, 1), 0xdef);
    app.event(
        SimEvent::Telemetry {
            id: id(1, 1),
            payload: Arc::new(Pulse { decisions: 50 }),
        },
        Instant::now(),
    );
    start(&mut app, id(3, 1), 7);
    finish(&mut app, id(3, 1), 7, true);
    let text = draw(&mut app);
    assert!(text.contains("Workers"), "{text}");
    assert!(text.contains("w1"), "{text}");
    assert!(text.contains("w2"), "{text}");
    assert!(text.contains("Outcomes"), "{text}");
    assert!(text.contains("windows"), "{text}");
    assert!(text.contains("6. crash sweep"), "{text}");
    assert!(text.contains("none so far"), "{text}");
}

#[test]
fn clicking_a_worker_opens_its_live_run_and_esc_returns() {
    let (mut app, _) = app();
    start(&mut app, id(1, 4), 0xabc);
    draw(&mut app);
    click(&mut app, "worker.1.4");
    assert_eq!(app.tab, 2);
    let text = draw(&mut app);
    assert!(text.contains("LIVE"), "{text}");
    assert!(text.contains("Overview"), "{text}");
    // The run is watched while open.
    app.tick(Instant::now());
    assert!(app.interest.watching(id(1, 4)));
    key(&mut app, KeyCode::Escape);
    assert_eq!(app.tab, 0);
    assert_eq!(app.tabs.len(), 2);
}

#[test]
fn a_failure_is_toasted_reproduced_and_then_shrunk() {
    let (mut app, fake) = app();
    start(&mut app, id(1, 1), 42);
    finish(&mut app, id(1, 1), 42, false);
    assert_eq!(app.toasts.len(), 1);
    app.tick(Instant::now());
    assert_eq!(fake.0.lock().unwrap().replays, [(42, false)]);
    let job = app.store.failures[0].confirming.unwrap();
    app.event(
        SimEvent::RunStarted {
            id: job,
            seed: 42,
            props: vec![],
            steps: None,
            worker: 0,
            kind: RunKind::Replay,
            epoch_offset: 3,
        },
        Instant::now(),
    );
    app.event(
        SimEvent::RunFinished {
            id: job,
            seed: 42,
            success: false,
            report: String::new(),
            steps: 1,
            real_time: Duration::ZERO,
            sim_time: Duration::ZERO,
        },
        Instant::now(),
    );
    assert_eq!(app.store.failures[0].reproduced, Some(true));
    app.tick(Instant::now());
    assert_eq!(fake.0.lock().unwrap().shrinks, [42]);
    key(&mut app, KeyCode::Char('f'));
    let text = draw(&mut app);
    assert!(text.contains("Failures by cause"), "{text}");
    assert!(text.contains("crashed server"), "{text}");
    assert!(text.contains("shrinking"), "{text}");
}

#[test]
fn opening_a_finished_run_replays_it_and_the_transport_steps_it() {
    let (mut app, fake) = app();
    start(&mut app, id(1, 1), 99);
    finish(&mut app, id(1, 1), 99, true);
    app.act(Action::OpenRun(id(1, 1)));
    app.tick(Instant::now());
    // The cursor plus keyframes along the run's 400 decisions.
    let replays = fake.0.lock().unwrap().replays.len();
    assert!(replays > 1, "{replays}");
    let text = draw(&mut app);
    assert!(text.contains("REPLAY"), "{text}");
    assert!(text.contains("Timeline"), "{text}");
    assert!(text.contains("thread-1"), "{text}");
    click(&mut app, "transport.back");
    let cursor = &app.current_run().unwrap().travel.as_ref().unwrap().cursor;
    assert_eq!(cursor.target, 398);
    // Clicking an event on the timeline goes there.
    draw(&mut app);
    click(&mut app, "event.120");
    let travel = app.current_run().unwrap().travel.as_ref().unwrap();
    assert_eq!(travel.cursor.target, 120);
}

#[test]
fn pages_and_sim_views_drill_in_by_clicks() {
    let (mut app, _) = app();
    start(&mut app, id(1, 1), 5);
    app.act(Action::OpenRun(id(1, 1)));
    draw(&mut app);
    click(&mut app, "page.4");
    let text = draw(&mut app);
    assert!(text.contains("Kernel"), "{text}");
    assert!(text.contains("sleep"), "{text}");
    click(&mut app, "entity.p1");
    let text = draw(&mut app);
    assert!(text.contains("card of p1"), "{text}");
    // A second click opens it in full.
    click(&mut app, "entity.p1");
    assert_eq!(app.current_run().unwrap().entity.as_deref(), Some("p1"));
    key(&mut app, KeyCode::Escape);
    assert_eq!(app.current_run().unwrap().entity, None);
}

#[test]
fn the_palette_finds_commands_and_runs_them() {
    let (mut app, fake) = app();
    key(&mut app, KeyCode::Char(':'));
    for c in "paus".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    let text = draw(&mut app);
    assert!(text.contains("Pause or resume the fleet"), "{text}");
    key(&mut app, KeyCode::Enter);
    assert!(fake.0.lock().unwrap().paused);
    key(&mut app, KeyCode::Char(':'));
    for c in "parallel 6".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(fake.0.lock().unwrap().parallel, 6);
}

#[test]
fn hit_ids_map_to_actions() {
    assert_eq!(Action::from_hit("tab.3"), Some(Action::Tab(3)));
    assert_eq!(Action::from_hit("tab.3.close"), Some(Action::CloseTab(3)));
    assert_eq!(
        Action::from_hit("worker.2.9"),
        Some(Action::OpenRun(id(2, 9)))
    );
    assert_eq!(
        Action::from_hit("failure.1.shrink"),
        Some(Action::Shrink(1))
    );
    assert_eq!(Action::from_hit("event.77"), Some(Action::Seek(77)));
    assert_eq!(
        Action::from_hit("entity.p.1"),
        Some(Action::Select("p.1".into()))
    );
    assert_eq!(
        Action::from_hit("filter.personality:linux"),
        Some(Action::Filter(Some("personality:linux".into())))
    );
    assert_eq!(Action::from_hit("list.workers"), None);
}

#[test]
fn commands_parse() {
    assert_eq!(parse_command("goto 1,200"), Some(Action::Seek(1200)));
    assert_eq!(parse_command("back 5"), Some(Action::Step(-5)));
    assert_eq!(parse_command("break fail"), Some(Action::BreakFailure));
    assert_eq!(parse_command("break 40"), Some(Action::BreakDecision(40)));
    assert_eq!(
        parse_command("break crash"),
        Some(Action::BreakEvent("crash".into()))
    );
    assert_eq!(parse_command("pause"), None);
    assert!(fuzzy("pse", "pause the fleet").is_some());
    assert!(fuzzy("xyz", "pause").is_none());
}

#[test]
fn the_wheel_scrolls_the_list_under_the_pointer() {
    let (mut app, _) = app();
    for worker in 1..=60 {
        start(&mut app, id(worker, 1), worker);
    }
    draw(&mut app);
    let area = region(&app, "list.workers");
    app.input(
        &Event::Mouse(MouseEvent::new(
            MouseEventKind::ScrollDown,
            bmux_tui::geometry::Point::new(area.x + 2, area.y + 2),
        )),
        Instant::now(),
    );
    assert_eq!(app.scroll.get("workers"), Some(&WHEEL));
}

#[test]
fn dragging_a_divider_resizes_and_the_scrubber_seeks() {
    let (mut app, _) = app();
    start(&mut app, id(1, 1), 3);
    draw(&mut app);
    let handle = region(&app, "split.col.fleet");
    let at = |x| bmux_tui::geometry::Point::new(x, handle.y + 3);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
    ] {
        let x = if matches!(kind, MouseEventKind::Down(_)) {
            handle.x
        } else {
            WIDTH / 4
        };
        app.input(&Event::Mouse(MouseEvent::new(kind, at(x))), Instant::now());
    }
    assert_eq!(app.splits.get("col.fleet"), Some(&25));
    finish(&mut app, id(1, 1), 3, true);
    app.act(Action::OpenRun(id(1, 1)));
    draw(&mut app);
    let scrub = region(&app, "scrub");
    app.input(
        &Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            bmux_tui::geometry::Point::new(scrub.x + scrub.width / 2, scrub.y),
        )),
        Instant::now(),
    );
    let target = app
        .current_run()
        .unwrap()
        .travel
        .as_ref()
        .unwrap()
        .cursor
        .target;
    assert!((190..=210).contains(&target), "{target}");
}

#[test]
fn both_themes_draw() {
    let (mut app, _) = app();
    start(&mut app, id(1, 1), 5);
    key(&mut app, KeyCode::Char('t'));
    assert!(!app.theme.palette.dark);
    let text = draw(&mut app);
    assert!(text.contains("bmux sim"), "{text}");
    key(&mut app, KeyCode::Char('?'));
    let text = draw(&mut app);
    assert!(text.contains("Keys and pointer"), "{text}");
}

#[test]
#[ignore = "prints frames for a person to look at"]
fn print_frames() {
    let (mut app, _) = app();
    for worker in 1..=6 {
        start(&mut app, id(worker, 1), worker * 7919);
        app.event(
            SimEvent::Progress {
                id: id(worker, 1),
                step: worker * 13,
                progress: None,
            },
            Instant::now(),
        );
    }
    for n in 2..=8 {
        start(&mut app, id(9, n), n);
        finish(&mut app, id(9, n), n, n != 5);
    }
    app.tick(Instant::now());
    println!("{}", draw(&mut app));
    app.act(Action::Tab(1));
    println!("{}", draw(&mut app));
    app.act(Action::OpenRun(id(9, 4)));
    println!("{}", draw(&mut app));
}
