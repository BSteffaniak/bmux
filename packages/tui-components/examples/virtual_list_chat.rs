//! A chat-log demo of `VirtualListComponent` on the BMUX runtime: streaming
//! appends that stay followed at the bottom, history pages prepended as the
//! top edge approaches (the viewport stays still because positions are keyed),
//! keyed scroll motion driven by runtime timers, overscroll, and activation.
//!
//! Run with `cargo run -p bmux_tui_components --example virtual_list_chat --features virtual-list`.
//!
//! Keys: arrows / PageUp / PageDown / Home / End and the mouse wheel scroll;
//! drag the scrollbar; click a message to select it. `g` / `G` animate to the
//! top / bottom, `o` toggles overscroll, `m` toggles reduced motion, `s`
//! toggles streaming, and `q` or Ctrl-C quits.

use std::cell::{OnceCell, RefCell};
use std::io::{self, Write};
use std::rc::Rc;
use std::time::Duration;

use bmux_keyboard::KeyCode;
use bmux_tui::component::{Component, Constraints, Element, EventCx, LayoutCx, LayoutNode};
use bmux_tui::composition::{Column, Flex, TextBlock};
use bmux_tui::terminal_io::{TerminalGuard, terminal_size};
use bmux_tui::event::Event;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Color, Easing, Modifier, Style};
use bmux_tui::terminal::Terminal;
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::virtual_list::{
    ListAnchor, ListPosition, NoSourceMap, Overscroll, ScrollTarget, VirtualItemSource,
    VirtualListAction, VirtualListChrome, VirtualListComponent, VirtualListState,
};
use bmux_tui_runtime::{
    Program, Runtime, RuntimeConfig, RuntimeEvent, RuntimeHandle, TerminalInput, TerminalPresenter,
    TimerId, Update,
};

const FRAME: Duration = Duration::from_millis(16);
const MOTION: Duration = Duration::from_millis(250);
const STREAM_EVERY: Duration = Duration::from_millis(900);
/// Simulated latency of one history page fetch.
const PAGE_LATENCY: Duration = Duration::from_millis(150);
const PAGE: u64 = 25;
const FIRST_LIVE_ID: u64 = 2_000;
const INITIAL: u64 = 30;

const MOTION_TIMER: &str = "chat.motion";
const STREAM_TIMER: &str = "chat.stream";
const PAGE_TIMER: &str = "chat.page";

const AUTHORS: [&str; 4] = ["ada", "grace", "linus", "barbara"];
const WORDS: [&str; 16] = [
    "virtual", "lists", "measure", "every", "item", "exactly", "and", "keep", "keyed", "anchors",
    "stable", "while", "history", "streams", "into", "view",
];

struct ChatMessage {
    id: u64,
    author: &'static str,
    body: String,
}

/// Deterministic message content for an id.
fn message(id: u64) -> ChatMessage {
    let mut seed = id.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let words = 3 + next() % 40;
    let body = (0..words)
        .map(|_| WORDS[usize::try_from(next() % 16).unwrap_or(0)])
        .collect::<Vec<_>>()
        .join(" ");
    ChatMessage {
        id,
        author: AUTHORS[usize::try_from(next() % 4).unwrap_or(0)],
        body,
    }
}

/// Lazy item source: only measured or visible messages are constructed.
struct ChatSource<'a> {
    messages: &'a [ChatMessage],
    selected: Option<u64>,
}

impl VirtualItemSource<u64> for ChatSource<'_> {
    fn len(&self) -> usize {
        self.messages.len()
    }

    fn key(&self, index: usize) -> u64 {
        self.messages[index].id
    }

    fn layout_revision(&self, _index: usize) -> u64 {
        0
    }

    fn paint_revision(&self, index: usize) -> u64 {
        u64::from(self.selected == Some(self.messages[index].id))
    }

    fn build(&self, index: usize) -> Element<'_> {
        let message = &self.messages[index];
        let selected = self.selected == Some(message.id);
        let header = Line::from_spans(vec![
            Span::styled(
                message.author,
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  #{}", message.id),
                Style::new().fg(Color::BrightBlack),
            ),
        ]);
        let body = TextBlock::new(message.body.as_str())
            .wrap(TextWrap::Word)
            .style(if selected {
                Style::new().fg(Color::Black).bg(Color::Yellow)
            } else {
                Style::new()
            });
        Element::new(
            Column::new()
                .child(TextBlock::new(Text::from_lines(vec![header])))
                .child(body),
        )
    }
}

#[derive(Debug)]
enum Message {
    InputFailed(io::Error),
}

struct Chat {
    messages: Vec<ChatMessage>,
    next_id: u64,
    /// Oldest loaded id; history is exhausted at zero.
    oldest_id: u64,
    page_in_flight: bool,
    state: RefCell<VirtualListState<u64>>,
    selected: Option<u64>,
    layout: Option<LayoutNode>,
    handle: Rc<OnceCell<RuntimeHandle<Message>>>,
    streaming: bool,
    reduced_motion: bool,
}

impl Chat {
    fn new(handle: Rc<OnceCell<RuntimeHandle<Message>>>) -> Self {
        let first = FIRST_LIVE_ID - INITIAL;
        let mut state = VirtualListState::new(1);
        state.set_position(ListPosition::FollowBottom);
        state.set_overscroll(Overscroll::ViewportMinus(1));
        state.scroll.interaction.focused = true;
        Self {
            messages: (first..FIRST_LIVE_ID).map(message).collect(),
            next_id: FIRST_LIVE_ID,
            oldest_id: first,
            page_in_flight: false,
            state: RefCell::new(state),
            selected: None,
            layout: None,
            handle,
            streaming: true,
            reduced_motion: false,
        }
    }

    fn handle(&self) -> &RuntimeHandle<Message> {
        self.handle
            .get()
            .expect("runtime handle attached before running")
    }

    fn status(&self) -> String {
        let state = self.state.borrow();
        let position = match state.position() {
            ListPosition::Top => "top",
            ListPosition::FollowBottom => "following",
            ListPosition::Anchored(_) => "anchored",
            ListPosition::Overscrolled(_) => "overscrolled",
        };
        format!(
            " {} messages | {position} | stream {} | motion {} | overscroll {} | selected {} \
             | g/G animate  o overscroll  m motion  s stream  q quit",
            self.messages.len(),
            if self.streaming { "on" } else { "off" },
            if self.reduced_motion {
                "reduced"
            } else {
                "eased"
            },
            if state.overscroll() == Overscroll::None {
                "off"
            } else {
                "on"
            },
            self.selected
                .map_or_else(|| "-".to_owned(), |id| format!("#{id}")),
        )
    }

    fn tree<'a>(&'a self, source: &'a ChatSource<'a>) -> Column<'a> {
        Column::new()
            .child(
                TextBlock::new(self.status())
                    .wrap(TextWrap::None)
                    .style(Style::new().fg(Color::Black).bg(Color::White)),
            )
            .flex(Flex::new(
                1,
                VirtualListComponent::new("chat", source, &self.state)
                    .chrome(VirtualListChrome::interactive().wheel_rows(2)),
            ))
    }

    fn source(&self) -> ChatSource<'_> {
        ChatSource {
            messages: &self.messages,
            selected: self.selected,
        }
    }

    fn render(&mut self, cx: &mut PaintCx<'_, '_>) {
        let area = cx.area();
        let source = self.source();
        let tree = self.tree(&source);
        let layout = tree.layout(
            Constraints::tight(Size::new(area.width, area.height)),
            &mut LayoutCx::new(),
        );
        tree.paint(&layout, cx);
        drop(tree);
        self.layout = Some(layout);
    }

    fn route(&mut self, event: &Event) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        let source = self.source();
        let outcome = self
            .tree(&source)
            .event(event, layout, &mut EventCx::new(layout));
        let activated = self
            .state
            .get_mut()
            .drain_actions()
            .fold(None, |last, action| {
                if let VirtualListAction::Activated(id) = action {
                    Some(id)
                } else {
                    last
                }
            });
        if activated.is_some() {
            self.selected = activated;
        }
        outcome.is_handled() || activated.is_some()
    }

    fn animate(&mut self, target: ScrollTarget<u64>) {
        let duration = if self.reduced_motion {
            Duration::ZERO
        } else {
            MOTION
        };
        let now = self.handle().now();
        self.state
            .get_mut()
            .start_motion(target, Easing::EaseInOut, duration, now);
        self.drive_motion();
    }

    /// Commit the motion frame for the runtime clock and schedule the next.
    fn drive_motion(&mut self) {
        let handle = self.handle().clone();
        let timer = TimerId::new(MOTION_TIMER);
        match self
            .state
            .get_mut()
            .advance_motion(handle.now(), FRAME, &NoSourceMap)
        {
            Some(next) => handle.schedule_timer(timer, next),
            None => {
                let _ = handle.cancel_timer(&timer);
            }
        }
    }

    fn stream(&mut self) {
        let start = self.messages.len();
        self.messages.push(message(self.next_id));
        self.next_id += 1;
        self.state.get_mut().note_splice(start..start, 1);
        if self.streaming {
            self.handle()
                .schedule_timer_after(TimerId::new(STREAM_TIMER), STREAM_EVERY);
        }
    }

    /// Request a history page when fewer than one viewport of rows remain above.
    /// Paging policy belongs to the caller; the list reports exact edge rows.
    fn request_history(&mut self) {
        if self.page_in_flight || self.oldest_id == 0 {
            return;
        }
        let Some(resolved) = self.state.get_mut().resolved() else {
            return;
        };
        if resolved.edge_rows().before < resolved.viewport_height {
            self.page_in_flight = true;
            self.handle()
                .schedule_timer_after(TimerId::new(PAGE_TIMER), PAGE_LATENCY);
        }
    }

    fn prepend_history(&mut self) {
        self.page_in_flight = false;
        let start = self.oldest_id.saturating_sub(PAGE);
        let page: Vec<_> = (start..self.oldest_id).map(message).collect();
        let count = page.len();
        let state = self.state.get_mut();
        // `Top` always means row 0; pin the current first message instead so
        // the prepended page appears above the viewport.
        if state.position() == ListPosition::Top
            && let Some(first) = self.messages.first()
        {
            state.set_position(ListPosition::Anchored(ListAnchor::top(first.id)));
        }
        self.messages.splice(0..0, page);
        self.oldest_id = start;
        state.note_splice(0..0, count);
    }

    fn key(&mut self, event: &Event) -> Option<Update<Message>> {
        let Event::Key(stroke) = event else {
            return None;
        };
        match stroke.key {
            KeyCode::Char('c') if stroke.modifiers.ctrl => Some(Update::exit()),
            KeyCode::Char('q') => Some(Update::exit()),
            KeyCode::Char('g') => {
                self.animate(ScrollTarget::Top);
                Some(Update::redraw())
            }
            KeyCode::Char('G') => {
                self.animate(ScrollTarget::Bottom);
                Some(Update::redraw())
            }
            KeyCode::Char('o') => {
                let state = self.state.get_mut();
                state.set_overscroll(if state.overscroll() == Overscroll::None {
                    Overscroll::ViewportMinus(1)
                } else {
                    Overscroll::None
                });
                Some(Update::redraw())
            }
            KeyCode::Char('m') => {
                self.reduced_motion = !self.reduced_motion;
                Some(Update::redraw())
            }
            KeyCode::Char('s') => {
                self.streaming = !self.streaming;
                let timer = TimerId::new(STREAM_TIMER);
                if self.streaming {
                    self.handle().schedule_timer_after(timer, STREAM_EVERY);
                } else {
                    let _ = self.handle().cancel_timer(&timer);
                }
                Some(Update::redraw())
            }
            _ => None,
        }
    }
}

impl Program for Chat {
    type Message = Message;
    type Error = io::Error;

    fn update(&mut self, event: RuntimeEvent<Message>) -> io::Result<Update<Message>> {
        let update = match event {
            RuntimeEvent::Message(Message::InputFailed(error)) => return Err(error),
            RuntimeEvent::Timer(id) => match id.as_str() {
                MOTION_TIMER => {
                    self.drive_motion();
                    Update::redraw()
                }
                STREAM_TIMER => {
                    self.stream();
                    Update::redraw()
                }
                PAGE_TIMER => {
                    self.prepend_history();
                    Update::redraw()
                }
                _ => Update::none(),
            },
            RuntimeEvent::Terminal(event) => {
                if let Some(update) = self.key(&event) {
                    update
                } else if self.route(&event) || matches!(event, Event::Resize(_)) {
                    Update::redraw()
                } else {
                    Update::none()
                }
            }
        };
        self.request_history();
        Ok(update)
    }

    fn presentation_committed(
        &mut self,
        _report: bmux_tui_runtime::PresentReport,
    ) -> Update<Message> {
        // A frame may reveal the top edge without any input (for example after
        // a resize), so paging is also checked against each committed layout.
        self.request_history();
        Update::none()
    }
}

async fn run(writer: &mut impl Write, size: Size) -> io::Result<()> {
    let handle = Rc::new(OnceCell::new());
    let terminal = Terminal::new(writer, Rect::new(0, 0, size.width, size.height));
    let presenter = TerminalPresenter::new(terminal, Chat::render);
    let (runtime, runtime_handle) = Runtime::new(
        Chat::new(Rc::clone(&handle)),
        presenter,
        RuntimeConfig::default(),
    );
    let _ = handle.set(runtime_handle.clone());
    runtime_handle.schedule_timer_after(TimerId::new(STREAM_TIMER), STREAM_EVERY);
    let mut input = TerminalInput::start::<Chat>(runtime_handle, Message::InputFailed);
    let result = runtime.run().await;
    input.shutdown().await;
    match result {
        Ok(_) => Ok(()),
        Err(bmux_tui_runtime::RuntimeError::Program { error, .. }) => Err(error),
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, .. }) => Err(error),
    }
}

fn main() -> io::Result<()> {
    let tasks = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let size = terminal_size()?;
    let mut guard = TerminalGuard::enter(io::stdout())?;
    let result = tasks.block_on(run(guard.writer_mut().expect("guard owns stdout"), size));
    let restored = guard.leave().map(|_| ());
    result.and(restored)
}
