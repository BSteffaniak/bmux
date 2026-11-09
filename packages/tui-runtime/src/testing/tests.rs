use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use bmux_keyboard::KeyCode;
use bmux_tui::event::MouseEventKind;
use bmux_tui::hit::HitRegion;
use bmux_tui::paint::LocalRect;
use bmux_tui::style::Color;
use bmux_tui::text::{Line, Span};
use tokio::sync::oneshot;

use super::*;
use crate::{Command, Subscription, SubscriptionKey, Update};

/// Shared ordered log of program-observable runtime callbacks.
type Log = Rc<RefCell<Vec<String>>>;

/// An empty log, for comparisons that print the unexpected entries on failure.
const NO_ENTRIES: [&str; 0] = [];

/// Message type for the scenario program.
#[derive(Debug)]
enum Msg {
    Text(String),
    /// Start a keyed replace command that completes when the receiver fires.
    Replace(&'static str, oneshot::Receiver<String>),
    /// Cancel a keyed command.
    Cancel(&'static str),
    /// Start a command completing after a Tokio sleep.
    Sleep(Duration, String),
    /// Schedule a timer relative to the runtime clock.
    Timer(&'static str, Duration),
    /// Send `n` follow-up messages to itself.
    Chain(u32),
    /// Start a subscription emitting the given texts.
    Subscribe(Vec<&'static str>),
    /// Request exit.
    Quit,
}

struct Scenario {
    handle: RuntimeHandle<Msg>,
    log: Log,
    text: String,
    commits: u32,
    redraw_after_commit: bool,
}

impl Scenario {
    fn new(handle: &RuntimeHandle<Msg>, log: &Log) -> Self {
        Self {
            handle: handle.clone(),
            log: Rc::clone(log),
            text: String::new(),
            commits: 0,
            redraw_after_commit: false,
        }
    }

    fn record(&self, entry: impl Into<String>) {
        self.log.borrow_mut().push(entry.into());
    }
}

impl Program for Scenario {
    type Message = Msg;
    type Error = Infallible;

    fn presentation_committed(&mut self, report: PresentReport) -> Update<Msg> {
        self.commits += 1;
        self.record(format!(
            "committed {} full={}",
            self.commits, report.full_repaint
        ));
        if self.redraw_after_commit {
            self.redraw_after_commit = false;
            Update::redraw()
        } else {
            Update::none()
        }
    }

    fn update(&mut self, event: RuntimeEvent<Msg>) -> Result<Update<Msg>, Infallible> {
        let update = match event {
            RuntimeEvent::Terminal(Event::Key(stroke)) => {
                if let KeyCode::Char(ch) = stroke.key {
                    self.text.push(ch);
                }
                self.record(format!("key {:?}", stroke.key));
                Update::redraw()
            }
            RuntimeEvent::Terminal(Event::Resize(size)) => {
                self.record(format!("resize {}x{}", size.width, size.height));
                Update::redraw()
            }
            RuntimeEvent::Terminal(Event::Paste(text)) => {
                self.text.push_str(&text);
                self.record(format!("paste {text}"));
                Update::redraw()
            }
            RuntimeEvent::Terminal(other) => {
                self.record(format!("terminal {other:?}"));
                Update::none()
            }
            RuntimeEvent::Timer(id) => {
                self.record(format!("timer {}", id.as_str()));
                Update::redraw()
            }
            RuntimeEvent::Message(message) => match message {
                Msg::Text(text) => {
                    self.record(format!("message {text}"));
                    self.text.clone_from(&text);
                    Update::redraw()
                }
                Msg::Replace(key, receiver) => Update::none()
                    .with_command(Command::replace(CommandKey::new(key), async move {
                        receiver.await.ok().map(Msg::Text)
                    })),
                Msg::Cancel(key) => Update::none().with_command(Command::cancel(key.into())),
                Msg::Sleep(delay, text) => {
                    Update::none().with_command(Command::concurrent(async move {
                        tokio::time::sleep(delay).await;
                        Some(Msg::Text(text))
                    }))
                }
                Msg::Timer(id, delay) => {
                    self.handle.schedule_timer_after(TimerId::new(id), delay);
                    Update::none()
                }
                Msg::Chain(remaining) => {
                    self.record(format!("chain {remaining}"));
                    if remaining > 0 {
                        self.handle
                            .try_send(Msg::Chain(remaining - 1))
                            .expect("reliable capacity");
                    }
                    Update::none()
                }
                Msg::Subscribe(texts) => Update::none().with_subscription(Subscription::new(
                    SubscriptionKey::new("feed"),
                    move |sender| async move {
                        for text in texts {
                            sender
                                .send(Msg::Text(text.to_owned()))
                                .await
                                .expect("runtime open");
                        }
                    },
                )),
                Msg::Quit => Update::exit(),
            },
        };
        Ok(update)
    }
}

fn paint_scenario(program: &mut Scenario, cx: &mut PaintCx<'_, '_>) {
    let area = cx.area();
    program.record(format!("paint {}", program.text));
    cx.write_line(
        LocalRect::new(0, 0, area.width, 1),
        &Line::raw(program.text.clone()),
    );
}

fn harness_with(builder: HarnessBuilder<Scenario>) -> (ProgramHarness<Scenario>, Log) {
    let log: Log = Rc::default();
    let program_log = Rc::clone(&log);
    let harness = builder.build_with(
        move |handle| Scenario::new(handle, &program_log),
        paint_scenario,
    );
    (harness, log)
}

fn harness() -> (ProgramHarness<Scenario>, Log) {
    harness_with(ProgramHarness::builder(Size::new(20, 3)))
}

const fn key(ch: char) -> KeyStroke {
    KeyStroke::simple(KeyCode::Char(ch))
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.borrow_mut())
}

#[test]
fn many_events_between_frames_present_only_final_state_once() {
    let (mut harness, log) = harness();
    harness
        .push_key(key('a'))
        .push_key(key('b'))
        .push_message(Msg::Text("xy".into()))
        .push_key(key('z'))
        .push_paste("!");
    assert_eq!(harness.deliver(3).unwrap(), 3);
    assert_eq!(harness.scripted_pending(), 2);
    assert_eq!(
        harness.frames_presented(),
        0,
        "delivery alone never presents"
    );
    assert!(harness.redraw_pending());
    assert_eq!(harness.deliver(10).unwrap(), 2);
    assert_eq!(harness.frames_presented(), 0);

    let report = harness.present().unwrap().expect("redraw pending");
    assert!(report.full_repaint);
    assert_eq!(harness.frames_presented(), 1);
    assert!(!harness.redraw_pending());
    assert_eq!(harness.frame().row_text(0).as_deref(), Some("xyz!"));
    assert_eq!(
        take(&log),
        [
            "key Char('a')",
            "key Char('b')",
            "message xy",
            "key Char('z')",
            "paste !",
            "paint xyz!",
            "committed 1 full=true",
        ]
    );
    // Nothing is pending, so an explicit present does nothing.
    assert_eq!(harness.present().unwrap(), None);
    assert_eq!(harness.frames_presented(), 1);
}

#[test]
fn after_each_event_mode_presents_every_dirty_update() {
    let (mut harness, log) = harness_with(
        ProgramHarness::builder(Size::new(10, 1)).present_mode(PresentMode::AfterEachEvent),
    );
    harness
        .push_key(key('a'))
        .push_terminal(Event::Tick)
        .push_key(key('b'));
    assert_eq!(harness.settle().unwrap(), 3);
    // The tick requested no redraw, so only two frames were committed.
    assert_eq!(harness.frames_presented(), 2);
    assert_eq!(
        take(&log),
        [
            "key Char('a')",
            "paint a",
            "committed 1 full=true",
            "terminal Tick",
            "key Char('b')",
            "paint ab",
            "committed 2 full=false",
        ]
    );
}

#[test]
fn timers_fire_in_deadline_order_only_when_the_manual_clock_reaches_them() {
    let (mut harness, log) = harness();
    harness
        .push_message(Msg::Timer("late", Duration::from_millis(30)))
        .push_message(Msg::Timer("early", Duration::from_millis(10)))
        .push_message(Msg::Timer("tie-b", Duration::from_millis(20)))
        .push_message(Msg::Timer("tie-a", Duration::from_millis(20)));
    harness.settle().unwrap();
    let pending: Vec<_> = harness
        .pending_timers()
        .into_iter()
        .map(|timer| (timer.id.as_str().to_owned(), timer.remaining))
        .collect();
    assert_eq!(
        pending,
        [
            ("early".to_owned(), Duration::from_millis(10)),
            ("tie-a".to_owned(), Duration::from_millis(20)),
            ("tie-b".to_owned(), Duration::from_millis(20)),
            ("late".to_owned(), Duration::from_millis(30)),
        ]
    );
    take(&log);

    harness.advance(Duration::from_millis(9));
    assert_eq!(
        harness.settle().unwrap(),
        0,
        "no timer is due before its deadline"
    );
    assert_eq!(
        harness.timer_remaining(&TimerId::new("early")),
        Some(Duration::from_millis(1))
    );

    assert_eq!(harness.advance_to_next_timer(), Some(TimerId::new("early")));
    assert_eq!(harness.clock().elapsed(), Duration::from_millis(10));
    assert_eq!(harness.settle().unwrap(), 1);

    // Jumping past several deadlines delivers them by deadline, then key.
    assert_eq!(
        harness.advance_and_settle(Duration::from_secs(1)).unwrap(),
        3
    );
    assert_eq!(
        take(&log),
        ["timer early", "timer tie-a", "timer tie-b", "timer late"]
    );
    assert_eq!(harness.pending_timers(), Vec::new());
    assert_eq!(harness.next_timer(), None);
    assert_eq!(harness.stats().timers_delivered, 4);
}

#[test]
fn rescheduling_a_timer_replaces_its_deadline() {
    let (mut harness, log) = harness();
    harness.push_message(Msg::Timer("blink", Duration::from_millis(50)));
    harness.settle().unwrap();
    harness.advance(Duration::from_millis(40));
    harness.push_message(Msg::Timer("blink", Duration::from_millis(50)));
    harness.settle().unwrap();
    assert_eq!(
        harness.timer_remaining(&TimerId::new("blink")),
        Some(Duration::from_millis(50))
    );
    harness
        .advance_and_settle(Duration::from_millis(49))
        .unwrap();
    assert!(!take(&log).iter().any(|entry| entry.starts_with("timer")));
    harness
        .advance_and_settle(Duration::from_millis(1))
        .unwrap();
    assert_eq!(take(&log), ["timer blink"]);
}

#[test]
fn handle_timers_cancelled_before_their_deadline_never_fire() {
    let (mut harness, log) = harness();
    harness
        .handle()
        .schedule_timer_after(TimerId::new("t"), Duration::from_millis(5));
    assert!(harness.handle().cancel_timer(&TimerId::new("t")));
    harness.advance_and_settle(Duration::from_secs(1)).unwrap();
    assert_eq!(take(&log), NO_ENTRIES);
}

#[test]
fn keyed_replace_discards_the_superseded_completion() {
    let (mut harness, log) = harness();
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, second_rx) = oneshot::channel();
    harness
        .push_message(Msg::Replace("load", first_rx))
        .push_message(Msg::Replace("load", second_rx));
    harness.settle().unwrap();
    assert_eq!(harness.commands().active, [CommandKey::new("load")]);

    // The superseded command was aborted; releasing it produces nothing.
    let _ = first_tx.send("stale".into());
    harness.settle().unwrap();
    assert_eq!(take(&log), NO_ENTRIES);

    second_tx.send("fresh".into()).unwrap();
    assert_eq!(harness.settle().unwrap(), 1);
    assert_eq!(take(&log), ["message fresh"]);
    assert_eq!(harness.commands(), CommandSnapshot::default());
}

#[test]
fn keyed_cancel_prevents_delivery_of_the_pending_result() {
    let (mut harness, log) = harness();
    let (tx, rx) = oneshot::channel();
    harness
        .push_message(Msg::Replace("save", rx))
        .push_message(Msg::Cancel("save"));
    harness.settle().unwrap();
    assert_eq!(harness.commands().active, Vec::<CommandKey>::new());
    let _ = tx.send("too late".into());
    harness.settle().unwrap();
    assert_eq!(take(&log), NO_ENTRIES);
    assert_eq!(harness.stats().commands_cancelled, 1);
}

#[test]
fn command_sleeps_follow_the_paused_task_clock() {
    let (mut harness, log) = harness();
    harness.push_message(Msg::Sleep(Duration::from_millis(100), "woke".into()));
    harness.settle().unwrap();
    assert_eq!(harness.commands().tasks, 1);

    harness
        .advance_and_settle(Duration::from_millis(99))
        .unwrap();
    assert_eq!(take(&log), NO_ENTRIES, "sleep must not complete early");

    assert_eq!(
        harness
            .advance_and_settle(Duration::from_millis(1))
            .unwrap(),
        1
    );
    assert_eq!(take(&log), ["message woke"]);
    assert_eq!(harness.commands().tasks, 0);
}

#[test]
fn scripted_effect_results_and_command_results_share_one_frame() {
    let (mut harness, log) = harness();
    let (tx, rx) = oneshot::channel();
    harness.push_message(Msg::Replace("fetch", rx));
    harness.settle().unwrap();
    tx.send("from command".into()).unwrap();
    harness
        .push_key(key('k'))
        .push_message(Msg::Text("scripted".into()));
    // Scripted events are delivered before runtime-owned completions.
    assert_eq!(harness.settle().unwrap(), 3);
    harness.present().unwrap();
    assert_eq!(
        take(&log),
        [
            "key Char('k')",
            "message scripted",
            "message from command",
            "paint from command",
            "committed 1 full=true",
        ]
    );
}

#[test]
fn presentation_commit_hooks_run_in_production_order() {
    let log: Log = Rc::default();
    let program_log = Rc::clone(&log);
    let commit_log = Rc::clone(&log);
    let mut harness = ProgramHarness::builder(Size::new(8, 2))
        .on_commit(move |program: &mut Scenario, hits, focus| {
            commit_log.borrow_mut().push(format!(
                "observer hits={} focus={:?} commits={}",
                hits.regions().len(),
                focus.active().map(FocusId::as_str),
                program.commits
            ));
        })
        .build_with(
            move |handle| Scenario::new(handle, &program_log),
            |program: &mut Scenario, cx: &mut PaintCx<'_, '_>| {
                paint_scenario(program, cx);
                cx.push_hit(HitRegion::new("button", Rect::new(0, 1, 4, 1)).focusable(true));
            },
        );
    harness.program_mut().redraw_after_commit = true;
    harness.push_key(key('a'));
    harness.deliver(1).unwrap();
    harness.present().unwrap();
    // presentation_committed returned a redraw, which is pending for a later frame.
    assert!(harness.redraw_pending());
    harness.present().unwrap();
    assert!(!harness.redraw_pending());
    let entries = take(&log);
    assert_eq!(entries[0], "key Char('a')");
    assert_eq!(entries[1], "paint a");
    assert!(entries[2].starts_with("observer hits=1"));
    assert!(
        entries[2].ends_with("commits=0"),
        "observer precedes the hook"
    );
    assert_eq!(entries[3], "committed 1 full=true");
    assert_eq!(entries[4], "paint a");
    assert!(entries[5].ends_with("commits=1"));
    assert_eq!(entries[6], "committed 2 full=false");
    assert_eq!(entries.len(), 7);

    let frame = harness.frame();
    assert_eq!(frame.hits().regions().len(), 1);
    assert_eq!(
        frame
            .hits()
            .hit_test(Point::new(1, 1))
            .map(|hit| hit.id().as_str().to_owned()),
        Some("button".to_owned())
    );
}

#[test]
fn resize_follows_the_production_reset_path() {
    let (mut harness, log) = harness();
    harness.force_present().unwrap();
    assert_eq!(harness.frame().size(), Size::new(20, 3));
    harness.push_resize(Size::new(6, 2)).push_key(key('q'));
    harness.deliver(2).unwrap();
    assert_eq!(harness.resets(), [ResetReason::Resize]);
    let report = harness.present().unwrap().expect("resize redraws");
    assert!(report.full_repaint);
    let frame = harness.frame();
    assert_eq!(frame.size(), Size::new(6, 2));
    assert_eq!(frame.row(0).as_deref(), Some("q     "));
    assert_eq!(frame.rows(), ["q", ""]);
    assert!(take(&log).contains(&"resize 6x2".to_owned()));
}

#[test]
fn frame_helpers_measure_committed_cells_not_bytes() {
    struct Static;
    impl Program for Static {
        type Message = ();
        type Error = Infallible;
        fn update(&mut self, _: RuntimeEvent<()>) -> Result<Update<()>, Infallible> {
            Ok(Update::none())
        }
    }
    let mut harness = ProgramHarness::builder(Size::new(12, 2)).build(
        Static,
        |_: &mut Static, cx: &mut PaintCx<'_, '_>| {
            cx.write_line(LocalRect::new(0, 0, 12, 1), &Line::raw("界e\u{301}x界"));
            cx.write_line(
                LocalRect::new(0, 1, 12, 1),
                &Line::from_spans([Span::styled("red", Style::new().fg(Color::Red))]),
            );
            cx.set_cursor_local(3, 1, true);
        },
    );
    assert!(harness.frame().buffer().is_none());
    harness.force_present().unwrap();
    let frame = harness.frame();
    assert_eq!(frame.row_text(0).as_deref(), Some("界e\u{301}x界"));
    // The CJK leader spans columns 0-1, so the combining sequence starts at 2.
    assert_eq!(frame.find("e\u{301}x"), Some(Point::new(2, 0)));
    assert_eq!(frame.find_all("界"), [Point::new(0, 0), Point::new(4, 0)]);
    assert_eq!(frame.find("red"), Some(Point::new(0, 1)));
    assert!(!frame.contains("absent"));
    let red = Style::new().fg(Color::Red);
    assert_eq!(frame.style_at(0, 1), Some(red));
    let runs = frame.style_runs(1);
    assert_eq!(runs[0].start, 0);
    assert_eq!(runs[0].width, 3);
    assert_eq!(runs[0].text, "red");
    assert_eq!(runs[0].style, red);
    assert_eq!(runs[1].start, 3);
    assert_eq!(frame.text(), "界e\u{301}x界\nred");
    assert_eq!(
        frame
            .cursor()
            .map(|cursor| (cursor.position, cursor.visible)),
        Some((Point::new(3, 1), true))
    );
    assert_ne!(harness.last_output(), &[] as &[u8]);
}

#[test]
fn cadence_mode_presents_on_the_manual_clock() {
    let (mut harness, log) = harness_with(
        ProgramHarness::builder(Size::new(10, 1))
            .config(RuntimeConfig {
                frame_interval: Some(Duration::from_millis(16)),
                ..RuntimeConfig::default()
            })
            .present_mode(PresentMode::Cadence),
    );
    harness.push_key(key('a')).push_key(key('b'));
    harness.settle().unwrap();
    // The first frame is due immediately; the second waits for the interval.
    assert_eq!(harness.frames_presented(), 1);
    assert!(harness.redraw_pending());
    harness.advance(Duration::from_millis(15));
    assert_eq!(harness.present_if_due().unwrap(), None);
    harness.advance(Duration::from_millis(1));
    assert!(harness.present_if_due().unwrap().is_some());
    assert_eq!(
        take(&log),
        [
            "key Char('a')",
            "paint a",
            "committed 1 full=true",
            "key Char('b')",
            "paint ab",
            "committed 2 full=false",
        ]
    );
}

#[test]
fn handle_messages_and_subscriptions_flow_through_runtime_sources() {
    let (mut harness, log) = harness();
    harness.push_message(Msg::Chain(2));
    assert_eq!(harness.settle().unwrap(), 3);
    assert_eq!(take(&log), ["chain 2", "chain 1", "chain 0"]);

    harness.push_message(Msg::Subscribe(vec!["one", "two"]));
    harness.settle().unwrap();
    assert_eq!(take(&log), ["message one", "message two"]);
    assert_eq!(harness.stats().subscriptions_completed, 1);
}

#[test]
fn settle_reports_programs_that_never_go_idle() {
    let (mut harness, _log) =
        harness_with(ProgramHarness::builder(Size::new(4, 1)).settle_limit(10));
    harness.push_message(Msg::Chain(1_000));
    match harness.settle() {
        Err(HarnessError::Unsettled { delivered }) => assert_eq!(delivered, 10),
        other => panic!("expected unsettled, got {other:?}"),
    }
    assert_eq!(harness.delivered(), 10);
}

#[test]
fn exit_stops_delivery_but_allows_presenting_pending_state() {
    let (mut harness, log) = harness();
    harness
        .push_key(key('a'))
        .push_message(Msg::Quit)
        .push_key(key('b'));
    assert_eq!(harness.settle().unwrap(), 2);
    assert!(harness.exit_requested());
    assert_eq!(harness.scripted_pending(), 1);
    assert!(harness.present().unwrap().is_some());
    assert_eq!(harness.frame().row_text(0).as_deref(), Some("a"));
    assert_eq!(
        take(&log),
        ["key Char('a')", "paint a", "committed 1 full=true"]
    );
}

#[test]
fn mouse_events_and_injected_timers_keep_script_order() {
    let (mut harness, log) = harness();
    harness
        .push_mouse(MouseEvent::new(
            MouseEventKind::ScrollDown,
            Point::new(1, 1),
        ))
        .push_timer(TimerId::new("direct"));
    harness.settle().unwrap();
    let entries = take(&log);
    assert!(entries[0].starts_with("terminal Mouse"));
    assert_eq!(entries[1], "timer direct");
}

/// Program whose frame content depends only on the presentation clock.
struct ClockedSpinner {
    start: Instant,
}

impl Program for ClockedSpinner {
    type Message = ();
    type Error = Infallible;

    fn update(&mut self, _: RuntimeEvent<()>) -> Result<Update<()>, Infallible> {
        Ok(Update::redraw())
    }
}

fn paint_clocked_spinner(spinner: &mut ClockedSpinner, cx: &mut PaintCx<'_, '_>) {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    let layout_clock = cx.layout_cx().clock();
    let Some(clock) = cx.clock() else {
        cx.write_line(LocalRect::new(0, 0, 8, 1), &Line::raw("no clock"));
        return;
    };
    assert_eq!(layout_clock, Some(clock));
    let step = clock.step(spinner.start, Duration::from_millis(100));
    let glyph = FRAMES[usize::try_from(step % 4).unwrap()];
    cx.write_line(
        LocalRect::new(0, 0, 8, 1),
        &Line::raw(format!("{glyph} {step}")),
    );
    cx.push_hit(HitRegion::new(
        format!("step-{step}"),
        Rect::new(0, 0, 1, 1),
    ));
}

#[test]
fn presentation_clock_follows_the_manual_clock_once_per_frame() {
    let mut harness = ProgramHarness::builder(Size::new(8, 1)).build_with(
        |handle| ClockedSpinner {
            start: handle.now(),
        },
        paint_clocked_spinner,
    );
    for (advance_ms, text, hit) in [
        (0, "| 0", "step-0"),
        (120, "/ 1", "step-1"),
        (100, "- 2", "step-2"),
        (179, "\\ 3", "step-3"),
        (1, "| 4", "step-4"),
    ] {
        harness.advance(Duration::from_millis(advance_ms));
        harness.force_present().unwrap();
        let frame = harness.frame();
        assert_eq!(frame.row_text(0).as_deref(), Some(text));
        let hits = frame
            .hits()
            .regions()
            .iter()
            .map(|region| region.id.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(hits, vec![hit.to_owned()]);
    }
}
