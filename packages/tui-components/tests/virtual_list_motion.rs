//! Drives a runtime `Program` hosting a `VirtualListComponent` through keyed
//! scroll motions on the deterministic harness clock, asserting committed
//! frames.

use std::cell::RefCell;
use std::convert::Infallible;
use std::time::Duration;

use bmux_tui::component::{
    Component, Constraints, Element, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::event::{MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::Easing;
use bmux_tui::text::Line;
use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::virtual_list::{
    AnchorPolicy, ListPosition, NoSourceMap, ScrollTarget, VirtualItemSource, VirtualListChrome,
    VirtualListComponent, VirtualListState,
};
use bmux_tui_runtime::testing::ProgramHarness;
use bmux_tui_runtime::{Program, RuntimeEvent, RuntimeHandle, TimerId, Update};

const FRAME: Duration = Duration::from_millis(16);
const MOTION_TIMER: &str = "log.motion";

/// One single-row log line.
struct Row(String);

impl Component for Row {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("row"),
            LogicalSize::new(constraints.max_width(), 1),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        cx.write_line(LocalRect::new(0, 0, width, 1), &Line::raw(self.0.clone()));
    }
}

/// Lazy source over the program's lines.
struct Lines<'a>(&'a [u64]);

impl VirtualItemSource<u64> for Lines<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn key(&self, index: usize) -> u64 {
        self.0[index]
    }

    fn layout_revision(&self, _index: usize) -> u64 {
        0
    }

    fn build(&self, index: usize) -> Element<'_> {
        Element::new(Row(format!("item {}", self.0[index])))
    }
}

#[derive(Debug)]
enum Message {
    ScrollTo(ScrollTarget<u64>, Duration),
    Append(u64),
}

struct Log {
    lines: Vec<u64>,
    state: RefCell<VirtualListState<u64>>,
    layout: Option<LayoutNode>,
    handle: RuntimeHandle<Message>,
}

impl Log {
    fn new(handle: &RuntimeHandle<Message>, count: u64) -> Self {
        Self {
            lines: (0..count).collect(),
            state: RefCell::new(VirtualListState::new(0)),
            layout: None,
            handle: handle.clone(),
        }
    }

    fn component<'a>(
        lines: &'a Lines<'a>,
        state: &'a RefCell<VirtualListState<u64>>,
    ) -> VirtualListComponent<'a, u64, Lines<'a>> {
        VirtualListComponent::new("log", lines, state)
            .chrome(VirtualListChrome::interactive().scrollbar(ScrollbarAxisLayoutMode::Hidden))
    }

    fn render(&mut self, cx: &mut PaintCx<'_, '_>) {
        let area = cx.area();
        let lines = Lines(&self.lines);
        let component = Self::component(&lines, &self.state);
        let layout = component.layout(
            Constraints::tight(Size::new(area.width, area.height)),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, cx);
        self.layout = Some(layout);
    }

    /// Commit the current motion frame and schedule the next one with a keyed
    /// runtime timer read from the runtime clock.
    fn drive(&mut self) {
        let timer = TimerId::new(MOTION_TIMER);
        let now = self.handle.now();
        match self
            .state
            .get_mut()
            .advance_motion(now, FRAME, &NoSourceMap)
        {
            Some(next) => self.handle.schedule_timer(timer, next),
            None => {
                let _ = self.handle.cancel_timer(&timer);
            }
        }
    }
}

impl Program for Log {
    type Message = Message;
    type Error = Infallible;

    fn update(&mut self, event: RuntimeEvent<Message>) -> Result<Update<Message>, Infallible> {
        match event {
            RuntimeEvent::Message(Message::ScrollTo(target, duration)) => {
                let now = self.handle.now();
                self.state
                    .get_mut()
                    .start_motion(target, Easing::Linear, duration, now);
                self.drive();
            }
            RuntimeEvent::Message(Message::Append(count)) => {
                let start = self.lines.len();
                let first = self.lines.last().map_or(0, |last| last + 1);
                self.lines.extend(first..first + count);
                self.state
                    .get_mut()
                    .note_splice(start..start, usize::try_from(count).unwrap_or(0));
            }
            RuntimeEvent::Timer(id) if id.as_str() == MOTION_TIMER => self.drive(),
            RuntimeEvent::Terminal(event) => {
                let Some(layout) = self.layout.as_ref() else {
                    return Ok(Update::none());
                };
                let lines = Lines(&self.lines);
                let outcome = Self::component(&lines, &self.state).event(
                    &event,
                    layout,
                    &mut EventCx::new(layout),
                );
                if !outcome.needs_redraw() {
                    return Ok(Update::none());
                }
            }
            RuntimeEvent::Timer(_) => return Ok(Update::none()),
        }
        Ok(Update::redraw())
    }
}

fn harness(count: u64) -> ProgramHarness<Log> {
    ProgramHarness::builder(Size::new(20, 5))
        .build_with(|handle| Log::new(handle, count), Log::render)
}

fn first_row(harness: &ProgramHarness<Log>) -> String {
    harness.frame().row_text(0).unwrap_or_default()
}

fn present(harness: &mut ProgramHarness<Log>) -> String {
    harness.present().expect("presents");
    first_row(harness)
}

#[test]
fn motion_frames_commit_on_the_manual_clock() {
    let mut harness = harness(50);
    harness.force_present().expect("presents");
    assert_eq!(first_row(&harness), "item 0");

    harness.push_message(Message::ScrollTo(
        ScrollTarget::Key(30, AnchorPolicy::ItemTopAtTop),
        Duration::from_millis(100),
    ));
    harness.settle().expect("settles");
    let timers = harness.pending_timers();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].id.as_str(), MOTION_TIMER);
    assert_eq!(timers[0].remaining, FRAME);
    assert_eq!(present(&mut harness), "item 0");

    // A late timer samples the clock it is delivered at, not its deadline.
    harness
        .advance_and_settle(Duration::from_millis(48))
        .expect("settles");
    assert_eq!(present(&mut harness), "item 14");
    assert_eq!(harness.pending_timers()[0].remaining, FRAME);

    harness.advance_to_next_timer().expect("frame scheduled");
    harness.settle().expect("settles");
    assert_eq!(present(&mut harness), "item 19");

    harness
        .advance_and_settle(Duration::from_millis(36))
        .expect("settles");
    assert_eq!(present(&mut harness), "item 30");
    assert!(harness.pending_timers().is_empty());
    assert!(harness.program().state.borrow().motion().is_none());
    assert_eq!(harness.frames_presented(), 5);
    assert_eq!(
        harness.frame().rows(),
        ["item 30", "item 31", "item 32", "item 33", "item 34"]
    );
}

#[test]
fn motion_retargets_to_moved_content_and_yields_to_manual_input() {
    let mut harness = harness(50);
    harness.force_present().expect("presents");
    harness.push_message(Message::ScrollTo(
        ScrollTarget::Bottom,
        Duration::from_millis(100),
    ));
    harness
        .advance_and_settle(Duration::from_millis(50))
        .expect("settles");
    harness.settle().expect("settles");
    assert_eq!(present(&mut harness), "item 0");
    harness
        .advance_and_settle(Duration::from_millis(50))
        .expect("settles");
    assert_eq!(present(&mut harness), "item 22");

    // Content streamed in mid-motion moves the keyed target.
    harness.push_message(Message::Append(5));
    harness.settle().expect("settles");
    assert_eq!(present(&mut harness), "item 22");
    harness
        .advance_and_settle(Duration::from_millis(50))
        .expect("settles");
    harness.force_present().expect("presents");
    assert_eq!(harness.frame().row_text(4).as_deref(), Some("item 54"));
    assert_eq!(
        harness.program().state.borrow().position(),
        ListPosition::FollowBottom
    );

    // Manual wheel input cancels a running motion where it is.
    harness.push_message(Message::ScrollTo(
        ScrollTarget::Top,
        Duration::from_millis(100),
    ));
    harness.settle().expect("settles");
    harness
        .advance_and_settle(Duration::from_millis(30))
        .expect("settles");
    assert_eq!(present(&mut harness), "item 35");
    harness.push_mouse(MouseEvent::new(MouseEventKind::ScrollUp, Point::new(1, 1)));
    harness.settle().expect("settles");
    assert_eq!(present(&mut harness), "item 32");
    assert!(harness.program().state.borrow().motion().is_none());
    harness
        .advance_and_settle(Duration::from_millis(200))
        .expect("settles");
    harness.force_present().expect("presents");
    assert_eq!(first_row(&harness), "item 32");
    assert!(harness.pending_timers().is_empty());
}

#[test]
fn zero_duration_motion_is_immediate_for_reduced_motion() {
    let mut harness = harness(50);
    harness.force_present().expect("presents");
    harness.push_message(Message::ScrollTo(
        ScrollTarget::Key(10, AnchorPolicy::Center),
        Duration::ZERO,
    ));
    harness.settle().expect("settles");
    assert!(harness.pending_timers().is_empty());
    // Centering a one-row item in five rows puts it on row 2.
    assert_eq!(present(&mut harness), "item 8");
    assert_eq!(harness.frame().row_text(2).as_deref(), Some("item 10"));
}
