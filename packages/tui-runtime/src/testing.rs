//! Deterministic test harness for runtime [`Program`]s.
//!
//! Enabled by the non-default `testing` feature. Downstream crates opt in from
//! their test builds only:
//!
//! ```toml
//! [dev-dependencies]
//! bmux_tui_runtime = { workspace = true, features = ["testing"] }
//! ```
//!
//! [`ProgramHarness`] drives the production runtime scheduler synchronously.
//! Scripted terminal events and messages pass through the same update path the
//! running runtime uses, so presenter observation, resize handling, command and
//! subscription scheduling, lifecycle, redraw latching, presentation, and the
//! [`Program::presentation_committed`] hook behave exactly as in production.
//! Only time and task execution are substituted:
//!
//! * A [`ManualClock`] replaces the system clock for timer deadlines and
//!   presentation cadence. It moves only when the test calls
//!   [`ProgramHarness::advance`].
//! * Commands and subscriptions run on a harness-owned current-thread Tokio
//!   runtime whose timer clock is paused and advanced together with the manual
//!   clock. Work runs only when the harness pumps it, so futures blocked on
//!   test-controlled channels or Tokio sleeps complete deterministically.
//!
//! Presentation is explicit: any number of events can be delivered between
//! frames, then [`ProgramHarness::present`] commits one frame into an
//! in-memory terminal whose committed buffer, cursor, hit map, selection scene,
//! semantics, focus, and [`PresentReport`] are inspected through
//! [`FrameView`]. [`PresentMode`] optionally presents automatically after each
//! delivered event or at the configured cadence.
//!
//! ```
//! use std::convert::Infallible;
//!
//! use bmux_tui::event::Event;
//! use bmux_tui::geometry::Size;
//! use bmux_tui::paint::LocalRect;
//! use bmux_tui::text::Line;
//! use bmux_tui_runtime::testing::ProgramHarness;
//! use bmux_tui_runtime::{Program, RuntimeEvent, Update};
//!
//! #[derive(Default)]
//! struct Counter(u32);
//!
//! impl Program for Counter {
//!     type Message = u32;
//!     type Error = Infallible;
//!
//!     fn update(&mut self, event: RuntimeEvent<u32>) -> Result<Update<u32>, Infallible> {
//!         if let RuntimeEvent::Message(step) = event {
//!             self.0 += step;
//!         }
//!         Ok(Update::redraw())
//!     }
//! }
//!
//! let mut harness = ProgramHarness::builder(Size::new(12, 1)).build(
//!     Counter::default(),
//!     |counter: &mut Counter, cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
//!         cx.write_line(LocalRect::new(0, 0, 12, 1), &Line::raw(format!("count {}", counter.0)));
//!     },
//! );
//! harness.push_message(1).push_message(2).push_message(3);
//! assert_eq!(harness.deliver(3).unwrap(), 3);
//! assert_eq!(harness.frames_presented(), 0);
//! harness.present().unwrap();
//! assert_eq!(harness.frame().row_text(0).as_deref(), Some("count 6"));
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use bmux_keyboard::KeyStroke;
use bmux_terminal_protocol::pointer_output::PointerShapeCapability;
use bmux_tui::buffer::{Buffer, Cell};
use bmux_tui::event::{Event, MouseEvent};
use bmux_tui::focus::{FocusId, FocusTrap};
use bmux_tui::frame::Cursor;
use bmux_tui::geometry::{Point, Rect, Size};
use bmux_tui::hit::HitMap;
use bmux_tui::paint::PaintCx;
use bmux_tui::selection::SelectionScene;
use bmux_tui::semantic::SemanticScene;
use bmux_tui::style::Style;
use bmux_tui::terminal::Terminal;

pub use crate::clock::ManualClock;
use crate::clock::RuntimeClock;
use crate::config::RuntimeConfig;
use crate::ids::{CommandKey, TimerId};
use crate::presenter::{PresentReport, Presenter, ResetReason};
use crate::program::{Program, RuntimeEvent};
use crate::runtime::{Runtime, RuntimeHandle};
use crate::stats::RuntimeStats;
use crate::terminal_presenter::TerminalPresenter;

#[cfg(test)]
mod tests;

/// Cooperative scheduler turns run by one [`ProgramHarness::pump`].
///
/// Each turn lets the current-thread scheduler poll every runnable task, so
/// this bounds chains of ready work rather than wall time.
const PUMP_TURNS: usize = 32;

/// Default bound on events delivered by one [`ProgramHarness::settle`].
pub const DEFAULT_SETTLE_LIMIT: usize = 10_000;

type RenderFn<P> = Box<dyn FnMut(&mut P, &mut PaintCx<'_, '_>)>;
type CommitFn<P> = Box<dyn FnMut(&mut P, &HitMap, &FocusTrap)>;

/// When the harness presents frames without an explicit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PresentMode {
    /// Present only on [`ProgramHarness::present`] or
    /// [`ProgramHarness::present_if_due`].
    #[default]
    Manual,
    /// Present after every delivered event that leaves a redraw pending,
    /// ignoring the configured frame cadence.
    AfterEachEvent,
    /// Present after a delivered event only when the production cadence rule
    /// (`RuntimeConfig::frame_interval` measured on the manual clock) permits.
    Cadence,
}

/// Failure reported by a harness operation.
#[derive(Debug)]
pub enum HarnessError<E> {
    /// [`Program::update`] returned an error.
    Program(E),
    /// The in-memory presenter failed.
    Presenter(io::Error),
    /// [`ProgramHarness::settle`] delivered its whole bound and work remained.
    Unsettled {
        /// Number of events delivered before giving up.
        delivered: usize,
    },
}

impl<E: fmt::Display> fmt::Display for HarnessError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Program(error) => write!(f, "program update failed: {error}"),
            Self::Presenter(error) => write!(f, "presentation failed: {error}"),
            Self::Unsettled { delivered } => {
                write!(f, "program did not settle after {delivered} events")
            }
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for HarnessError<E> {}

/// One pending keyed timer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTimer {
    /// Timer key.
    pub id: TimerId,
    /// Absolute deadline on the harness clock.
    pub deadline: Instant,
    /// Time remaining until the deadline, zero when already due.
    pub remaining: Duration,
}

/// Snapshot of runtime-owned keyed command work.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandSnapshot {
    /// Keys whose latest generation is active, including finished work whose
    /// completion has not yet been delivered.
    pub active: Vec<CommandKey>,
    /// Keys waiting behind active work or capacity.
    pub queued: Vec<CommandKey>,
    /// Spawned keyed and concurrent tasks not yet collected.
    pub tasks: usize,
}

/// Contiguous cells on one row sharing a style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRun {
    /// First column of the run.
    pub start: u16,
    /// Number of columns in the run.
    pub width: u16,
    /// Shared cell style.
    pub style: Style,
    /// Concatenated cell symbols.
    pub text: String,
}

/// Configures and builds a [`ProgramHarness`].
pub struct HarnessBuilder<P: Program> {
    size: Size,
    config: RuntimeConfig,
    mode: PresentMode,
    settle_limit: usize,
    commit: Option<CommitFn<P>>,
}

impl<P: Program> HarnessBuilder<P> {
    /// Replace the runtime configuration.
    ///
    /// The default disables frame cadence (`frame_interval: None`) and the
    /// wall-time processing budget is irrelevant because the harness delivers
    /// one event per step.
    #[must_use]
    pub const fn config(mut self, config: RuntimeConfig) -> Self {
        self.config = config;
        self
    }

    /// Select automatic presentation behavior.
    #[must_use]
    pub const fn present_mode(mut self, mode: PresentMode) -> Self {
        self.mode = mode;
        self
    }

    /// Bound the number of events one [`ProgramHarness::settle`] may deliver.
    #[must_use]
    pub const fn settle_limit(mut self, limit: usize) -> Self {
        self.settle_limit = limit;
        self
    }

    /// Observe the exact committed hit map and focus state after each
    /// successful presentation, as [`TerminalPresenter::with_commit`] does.
    ///
    /// The observer runs before [`Program::presentation_committed`].
    #[must_use]
    pub fn on_commit(mut self, commit: impl FnMut(&mut P, &HitMap, &FocusTrap) + 'static) -> Self {
        self.commit = Some(Box::new(commit));
        self
    }

    /// Build a harness around an existing program.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`Self::build_with`].
    #[must_use]
    pub fn build(
        self,
        program: P,
        render: impl FnMut(&mut P, &mut PaintCx<'_, '_>) + 'static,
    ) -> ProgramHarness<P> {
        self.build_with(|_handle| program, render)
    }

    /// Build a harness, constructing the program from the runtime handle.
    ///
    /// Programs that schedule timers or send messages through a retained
    /// [`RuntimeHandle`] receive the harness-controlled handle here.
    ///
    /// # Panics
    ///
    /// Panics when the operating system cannot provide the current-thread task
    /// runtime the harness uses for commands and subscriptions.
    #[must_use]
    pub fn build_with(
        self,
        make_program: impl FnOnce(&RuntimeHandle<P::Message>) -> P,
        render: impl FnMut(&mut P, &mut PaintCx<'_, '_>) + 'static,
    ) -> ProgramHarness<P> {
        let clock = ManualClock::new();
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("current-thread test runtime");
        let mut terminal = Terminal::new(
            Vec::new(),
            Rect::new(0, 0, self.size.width, self.size.height),
        );
        // Environment-detected pointer support would make output bytes host-dependent.
        terminal.set_pointer_capability(PointerShapeCapability::Unsupported);
        let commit: CommitFn<P> = self
            .commit
            .unwrap_or_else(|| Box::new(|_: &mut P, _: &HitMap, _: &FocusTrap| {}));
        let render: RenderFn<P> = Box::new(render);
        let presenter = HarnessPresenter {
            inner: TerminalPresenter::with_commit(terminal, render, commit),
            reports: Vec::new(),
            resets: Vec::new(),
        };
        let (runtime, handle) = {
            let _context = tokio.enter();
            Runtime::new_with(self.config, RuntimeClock::Manual(clock.clone()), |handle| {
                (make_program(handle), presenter)
            })
        };
        ProgramHarness {
            runtime,
            handle,
            script: VecDeque::new(),
            clock,
            mode: self.mode,
            settle_limit: self.settle_limit,
            delivered: 0,
            tokio,
        }
    }
}

/// In-memory presenter recording each committed report.
struct HarnessPresenter<P> {
    inner: TerminalPresenter<Vec<u8>, RenderFn<P>, CommitFn<P>>,
    reports: Vec<PresentReport>,
    resets: Vec<ResetReason>,
}

impl<P> Presenter<P> for HarnessPresenter<P> {
    type Error = io::Error;

    fn resize(&mut self, size: Size) {
        Presenter::<P>::resize(&mut self.inner, size);
    }

    fn reset(&mut self, reason: ResetReason) {
        self.resets.push(reason);
        Presenter::<P>::reset(&mut self.inner, reason);
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        self.inner.terminal_mut().writer_mut().clear();
        let report = Presenter::<P>::present(&mut self.inner, program)?;
        self.reports.push(report);
        Ok(report)
    }

    fn observe_terminal_event(&mut self, event: &Event) -> bool {
        Presenter::<P>::observe_terminal_event(&mut self.inner, event)
    }

    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        Presenter::<P>::set_pointer_context(&mut self.inner, context);
    }
}

/// Deterministic, synchronously stepped driver for one [`Program`].
///
/// See the [module documentation](self) for the execution model.
pub struct ProgramHarness<P: Program> {
    // Declared before `tokio` so runtime-owned tasks are released while the
    // task runtime still exists.
    runtime: Runtime<P, HarnessPresenter<P>>,
    handle: RuntimeHandle<P::Message>,
    script: VecDeque<RuntimeEvent<P::Message>>,
    clock: ManualClock,
    mode: PresentMode,
    settle_limit: usize,
    delivered: usize,
    tokio: tokio::runtime::Runtime,
}

impl<P: Program> ProgramHarness<P> {
    /// Start configuring a harness presenting into a terminal of `size`.
    #[must_use]
    pub fn builder(size: Size) -> HarnessBuilder<P> {
        HarnessBuilder {
            size,
            config: RuntimeConfig {
                frame_interval: None,
                ..RuntimeConfig::default()
            },
            mode: PresentMode::Manual,
            settle_limit: DEFAULT_SETTLE_LIMIT,
            commit: None,
        }
    }

    // ----- Program and runtime access -------------------------------------------------------

    /// Return the program.
    #[must_use]
    pub const fn program(&self) -> &P {
        self.runtime.harness_program()
    }

    /// Return the program mutably, for setup or white-box assertions.
    ///
    /// Mutations here bypass `update`; request a redraw explicitly when they
    /// change presented state.
    pub const fn program_mut(&mut self) -> &mut P {
        self.runtime.harness_program_mut()
    }

    /// Return the runtime admission handle bound to this harness.
    ///
    /// Messages, terminal events, and timers admitted through the handle are
    /// delivered by the runtime's fair source rotation after scripted events.
    #[must_use]
    pub const fn handle(&self) -> &RuntimeHandle<P::Message> {
        &self.handle
    }

    /// Return a point-in-time runtime statistics snapshot.
    #[must_use]
    pub fn stats(&self) -> RuntimeStats {
        self.runtime.harness_stats()
    }

    /// Return whether an update requested graceful exit or abort.
    ///
    /// Once set, delivery stops as in the running runtime; presentation of
    /// pending state remains available.
    #[must_use]
    pub const fn exit_requested(&self) -> bool {
        self.runtime.harness_exiting()
    }

    /// Return the number of events delivered to `update` so far.
    #[must_use]
    pub const fn delivered(&self) -> usize {
        self.delivered
    }

    // ----- Clock and timers -----------------------------------------------------------------

    /// Return the shared manual clock.
    #[must_use]
    pub const fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// Return the current harness instant.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    /// Advance the manual clock and the paused task clock by `delta`.
    ///
    /// Nothing is delivered; timers that became due are delivered by later
    /// steps. Commands blocked on Tokio sleeps become runnable at the next pump.
    pub fn advance(&mut self, delta: Duration) -> &mut Self {
        self.clock.advance(delta);
        self.tokio.block_on(tokio::time::advance(delta));
        self
    }

    /// Advance exactly to the earliest pending timer deadline.
    ///
    /// Returns the timer that is now due, or `None` when no timer is pending.
    pub fn advance_to_next_timer(&mut self) -> Option<TimerId> {
        let next = self.next_timer()?;
        if !next.remaining.is_zero() {
            self.advance(next.remaining);
        }
        Some(next.id)
    }

    /// Return pending timers ordered by deadline, then key.
    ///
    /// This is also the order in which due timers are delivered.
    #[must_use]
    pub fn pending_timers(&self) -> Vec<PendingTimer> {
        let now = self.clock.now();
        let mut timers: Vec<_> = self
            .runtime
            .harness_timers()
            .into_iter()
            .map(|(id, deadline)| PendingTimer {
                id,
                deadline,
                remaining: deadline.saturating_duration_since(now),
            })
            .collect();
        timers.sort_by(|left, right| {
            left.deadline
                .cmp(&right.deadline)
                .then_with(|| left.id.cmp(&right.id))
        });
        timers
    }

    /// Return the earliest pending timer.
    #[must_use]
    pub fn next_timer(&self) -> Option<PendingTimer> {
        self.pending_timers().into_iter().next()
    }

    /// Return time remaining until the timer `id` is due, if it is pending.
    #[must_use]
    pub fn timer_remaining(&self, id: &TimerId) -> Option<Duration> {
        self.runtime
            .harness_timers()
            .get(id)
            .map(|deadline| deadline.saturating_duration_since(self.clock.now()))
    }

    // ----- Commands -------------------------------------------------------------------------

    /// Return runtime-owned keyed command state.
    #[must_use]
    pub fn commands(&self) -> CommandSnapshot {
        let (active, queued, tasks) = self.runtime.harness_command_keys();
        CommandSnapshot {
            active,
            queued,
            tasks,
        }
    }

    /// Run spawned command and subscription work until it is blocked.
    ///
    /// Steps pump automatically before servicing runtime sources; call this
    /// directly to observe command state without delivering anything.
    pub fn pump(&mut self) -> &mut Self {
        self.tokio.block_on(async {
            for _ in 0..PUMP_TURNS {
                tokio::task::yield_now().await;
            }
        });
        self
    }

    // ----- Scripted input -------------------------------------------------------------------

    /// Script one terminal event.
    pub fn push_terminal(&mut self, event: Event) -> &mut Self {
        self.script.push_back(RuntimeEvent::Terminal(event));
        self
    }

    /// Script one key stroke.
    pub fn push_key(&mut self, stroke: KeyStroke) -> &mut Self {
        self.push_terminal(Event::Key(stroke))
    }

    /// Script one mouse event.
    pub fn push_mouse(&mut self, mouse: MouseEvent) -> &mut Self {
        self.push_terminal(Event::Mouse(mouse))
    }

    /// Script one terminal resize. The in-memory terminal follows the
    /// production resize path when the event is delivered.
    pub fn push_resize(&mut self, size: Size) -> &mut Self {
        self.push_terminal(Event::Resize(size))
    }

    /// Script one bracketed paste.
    pub fn push_paste(&mut self, text: impl Into<String>) -> &mut Self {
        self.push_terminal(Event::Paste(text.into()))
    }

    /// Script one application message, such as a command or effect result.
    pub fn push_message(&mut self, message: P::Message) -> &mut Self {
        self.script.push_back(RuntimeEvent::Message(message));
        self
    }

    /// Script one timer delivery regardless of pending deadlines.
    ///
    /// Prefer [`Self::advance`] for timers the program scheduled; this injects
    /// a `RuntimeEvent::Timer` directly.
    pub fn push_timer(&mut self, id: TimerId) -> &mut Self {
        self.script.push_back(RuntimeEvent::Timer(id));
        self
    }

    /// Return the number of scripted events not yet delivered.
    #[must_use]
    pub fn scripted_pending(&self) -> usize {
        self.script.len()
    }

    // ----- Delivery -------------------------------------------------------------------------

    /// Deliver at most one event.
    ///
    /// Scripted events are delivered first, in the order they were pushed.
    /// When the script is empty, spawned work is pumped and one event is taken
    /// from the runtime's own fair rotation: due timers, completed commands,
    /// subscription messages, and handle-admitted messages and terminal events.
    /// Returns `false` when nothing was ready or exit was requested.
    ///
    /// # Errors
    ///
    /// * [`HarnessError::Program`] when the update fails.
    /// * [`HarnessError::Presenter`] when an automatic presentation fails.
    pub fn step(&mut self) -> Result<bool, HarnessError<P::Error>> {
        if self.runtime.harness_exiting() {
            return Ok(false);
        }
        let delivered = if let Some(event) = self.script.pop_front() {
            let _context = self.tokio.enter();
            self.runtime
                .harness_apply(event)
                .map_err(HarnessError::Program)?;
            true
        } else {
            self.pump();
            let _context = self.tokio.enter();
            self.runtime
                .harness_step_ready()
                .map_err(HarnessError::Program)?
        };
        if !delivered {
            return Ok(false);
        }
        self.delivered = self.delivered.saturating_add(1);
        match self.mode {
            PresentMode::Manual => {}
            PresentMode::AfterEachEvent => {
                self.present()?;
            }
            PresentMode::Cadence => {
                self.present_if_due()?;
            }
        }
        Ok(true)
    }

    /// Deliver up to `count` events without presenting (in manual mode).
    ///
    /// Returns the number delivered, which is smaller than `count` when
    /// nothing else was ready.
    ///
    /// # Errors
    ///
    /// Returns the first error from [`Self::step`].
    pub fn deliver(&mut self, count: usize) -> Result<usize, HarnessError<P::Error>> {
        let mut delivered = 0;
        while delivered < count && self.step()? {
            delivered += 1;
        }
        Ok(delivered)
    }

    /// Deliver events until nothing is ready, returning the number delivered.
    ///
    /// Time does not advance; timers that are not yet due stay pending.
    ///
    /// # Errors
    ///
    /// * [`HarnessError::Unsettled`] when the settle limit is reached.
    /// * Any error from [`Self::step`].
    pub fn settle(&mut self) -> Result<usize, HarnessError<P::Error>> {
        let mut delivered = 0;
        while self.step()? {
            delivered += 1;
            if delivered >= self.settle_limit {
                return Err(HarnessError::Unsettled { delivered });
            }
        }
        Ok(delivered)
    }

    /// Advance time by `delta`, then settle.
    ///
    /// # Errors
    ///
    /// Returns any error from [`Self::settle`].
    pub fn advance_and_settle(&mut self, delta: Duration) -> Result<usize, HarnessError<P::Error>> {
        self.advance(delta);
        self.settle()
    }

    // ----- Presentation ---------------------------------------------------------------------

    /// Return whether a redraw is pending.
    #[must_use]
    pub fn redraw_pending(&self) -> bool {
        self.runtime.harness_invalidation().0
    }

    /// Return whether a backend reset is pending for the next presentation.
    #[must_use]
    pub fn reset_pending(&self) -> bool {
        self.runtime.harness_invalidation().1
    }

    /// Present one frame when a redraw is pending, ignoring cadence.
    ///
    /// The presentation uses the production path: pending resize and reset are
    /// applied, the frame is drawn into the in-memory terminal, the commit
    /// observer runs, and then [`Program::presentation_committed`] runs and its
    /// update is applied. Returns the committed report, or `None` when no
    /// redraw was pending.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Presenter`] when presentation fails.
    pub fn present(&mut self) -> Result<Option<PresentReport>, HarnessError<P::Error>> {
        let _context = self.tokio.enter();
        let presented = self
            .runtime
            .harness_present_dirty()
            .map_err(HarnessError::Presenter)?;
        Ok(presented.then(|| self.last_report()).flatten())
    }

    /// Request a redraw, then present.
    ///
    /// # Panics
    ///
    /// Panics if an asynchronous presentation is still in flight, which the
    /// in-memory presenter never leaves behind.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Presenter`] when presentation fails.
    pub fn force_present(&mut self) -> Result<PresentReport, HarnessError<P::Error>> {
        self.runtime.harness_request_redraw();
        Ok(self.present()?.expect("a requested redraw always presents"))
    }

    /// Present only when a redraw is pending and the configured frame interval
    /// has elapsed on the manual clock since the previous presentation.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Presenter`] when presentation fails.
    pub fn present_if_due(&mut self) -> Result<Option<PresentReport>, HarnessError<P::Error>> {
        let _context = self.tokio.enter();
        let presented = self
            .runtime
            .harness_present_due()
            .map_err(HarnessError::Presenter)?;
        Ok(presented.then(|| self.last_report()).flatten())
    }

    /// Return the number of committed presentations.
    #[must_use]
    pub const fn frames_presented(&self) -> usize {
        self.presenter().reports.len()
    }

    /// Return every committed presentation report in order.
    #[must_use]
    pub fn reports(&self) -> &[PresentReport] {
        &self.presenter().reports
    }

    /// Return the most recent committed presentation report.
    #[must_use]
    pub fn last_report(&self) -> Option<PresentReport> {
        self.presenter().reports.last().copied()
    }

    /// Return every backend reset requested so far, in order.
    #[must_use]
    pub fn resets(&self) -> &[ResetReason] {
        &self.presenter().resets
    }

    /// Return the bytes the most recent presentation wrote to the terminal.
    #[must_use]
    pub fn last_output(&self) -> &[u8] {
        self.presenter().inner.terminal().writer()
    }

    /// Return a view of the last committed frame.
    #[must_use]
    pub fn frame(&self) -> FrameView<'_> {
        FrameView {
            terminal: self.presenter().inner.terminal(),
            report: self.last_report(),
        }
    }

    const fn presenter(&self) -> &HarnessPresenter<P> {
        self.runtime.harness_presenter()
    }
}

/// Read-only view of the last committed frame of a [`ProgramHarness`].
///
/// All cell coordinates are terminal columns and rows. Text helpers read the
/// committed cell symbols, so wide graphemes occupy their leader column and
/// their continuation column contributes no text.
#[derive(Clone, Copy)]
pub struct FrameView<'a> {
    terminal: &'a Terminal<Vec<u8>>,
    report: Option<PresentReport>,
}

impl<'a> FrameView<'a> {
    /// Return the terminal size.
    #[must_use]
    pub const fn size(&self) -> Size {
        let area = self.terminal.area();
        Size::new(area.width, area.height)
    }

    /// Return the report of the presentation that committed this frame.
    #[must_use]
    pub const fn report(&self) -> Option<PresentReport> {
        self.report
    }

    /// Return the committed cell buffer, absent before the first presentation
    /// and after a resize until the next one.
    #[must_use]
    pub const fn buffer(&self) -> Option<&'a Buffer> {
        self.terminal.retained_buffer()
    }

    /// Return one committed cell.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&'a Cell> {
        self.buffer()?.get(Point::new(x, y))
    }

    /// Return the style of one committed cell.
    #[must_use]
    pub fn style_at(&self, x: u16, y: u16) -> Option<Style> {
        self.cell(x, y).map(|cell| cell.style)
    }

    /// Return the full symbols of row `y`, including trailing blanks.
    #[must_use]
    pub fn row(&self, y: u16) -> Option<String> {
        self.buffer()?.row_symbols(y)
    }

    /// Return the symbols of row `y` with trailing whitespace removed.
    #[must_use]
    pub fn row_text(&self, y: u16) -> Option<String> {
        self.row(y).map(|row| row.trim_end().to_owned())
    }

    /// Return every row with trailing whitespace removed.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        (0..self.size().height)
            .map(|y| self.row_text(y).unwrap_or_default())
            .collect()
    }

    /// Return the frame text: trimmed rows joined by newlines.
    ///
    /// Suitable for `insta::assert_snapshot!` or plain string comparison.
    #[must_use]
    pub fn text(&self) -> String {
        self.rows().join("\n")
    }

    /// Return whether any row contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.find(needle).is_some()
    }

    /// Return the cell position where the first occurrence of `needle` starts.
    ///
    /// Rows are searched top to bottom. Matches never span rows.
    #[must_use]
    pub fn find(&self, needle: &str) -> Option<Point> {
        self.find_all(needle).into_iter().next()
    }

    /// Return the cell positions of every non-overlapping occurrence of
    /// `needle`, in reading order.
    #[must_use]
    pub fn find_all(&self, needle: &str) -> Vec<Point> {
        let mut found = Vec::new();
        let Some(buffer) = self.buffer() else {
            return found;
        };
        if needle.is_empty() {
            return found;
        }
        let area = buffer.area();
        for y in area.y..area.bottom() {
            // Map byte offsets of the concatenated row back to the cell that
            // contributed them; widths come from committed cells, not text.
            let mut row = String::new();
            let mut starts = Vec::new();
            for x in area.x..area.right() {
                if let Some(cell) = buffer.get(Point::new(x, y)) {
                    starts.push((row.len(), x));
                    row.push_str(&cell.symbol);
                }
            }
            let mut from = 0;
            while let Some(offset) = row[from..].find(needle) {
                let byte = from + offset;
                if let Some(&(_, x)) = starts.iter().rev().find(|(start, _)| *start <= byte) {
                    found.push(Point::new(x, y));
                }
                from = byte + needle.len();
            }
        }
        found
    }

    /// Return row `y` split into runs of equally styled cells.
    #[must_use]
    pub fn style_runs(&self, y: u16) -> Vec<StyleRun> {
        let mut runs: Vec<StyleRun> = Vec::new();
        let Some(buffer) = self.buffer() else {
            return runs;
        };
        let area = buffer.area();
        if y < area.y || y >= area.bottom() {
            return runs;
        }
        for x in area.x..area.right() {
            let Some(cell) = buffer.get(Point::new(x, y)) else {
                continue;
            };
            match runs.last_mut() {
                Some(run) if run.style == cell.style => {
                    run.width = run.width.saturating_add(1);
                    run.text.push_str(&cell.symbol);
                }
                _ => runs.push(StyleRun {
                    start: x,
                    width: 1,
                    style: cell.style,
                    text: cell.symbol.clone(),
                }),
            }
        }
        runs
    }

    /// Return the committed cursor.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.terminal.cursor()
    }

    /// Return the committed interaction scene.
    #[must_use]
    pub const fn hits(&self) -> &'a HitMap {
        self.terminal.hits()
    }

    /// Return the committed selection scene.
    #[must_use]
    pub const fn selection(&self) -> &'a SelectionScene {
        self.terminal.selection()
    }

    /// Return the committed semantic scene.
    #[must_use]
    pub const fn semantics(&self) -> &'a SemanticScene {
        self.terminal.semantics()
    }

    /// Return the committed focus state.
    #[must_use]
    pub const fn focus(&self) -> &'a FocusTrap {
        self.terminal.focus()
    }

    /// Return the focused target of the committed frame.
    #[must_use]
    pub fn focused(&self) -> Option<&'a FocusId> {
        self.terminal.focused()
    }
}

impl fmt::Debug for FrameView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameView")
            .field("size", &self.size())
            .field("report", &self.report)
            .field("text", &self.text())
            .finish_non_exhaustive()
    }
}
