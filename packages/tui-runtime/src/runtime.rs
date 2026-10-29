//! Bounded fair event, timer, command, and presentation scheduler.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bmux_tui::event::Event;
use tokio::sync::{Notify, mpsc};

use crate::command_scheduler::CommandScheduler;
use crate::config::RuntimeConfig;
use crate::ids::{MessageKey, TimerId};
use crate::mailbox::{
    LatestReceiver, LatestSendError, LatestSendOutcome, LatestSender, ReliableSender, SendError,
    TrySendError, latest_channel, observe_receive, stats_snapshot, with_stats,
};
use crate::presenter::{Presenter, ResetReason};
use crate::program::{Invalidation, Lifecycle, Program, RuntimeEvent, Update};
use crate::stats::{
    Admitted, PresentationSample, QueueSource, RuntimeStats, TerminalDispatchRange, micros,
};
use crate::subscription_scheduler::SubscriptionScheduler;

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug)]
struct ControlState {
    timers: BTreeMap<TimerId, Instant>,
    dirty: bool,
    reset: bool,
    frame_interval: Option<Duration>,
    closed: bool,
}

#[derive(Debug)]
struct SharedControl {
    state: Mutex<ControlState>,
    notify: Notify,
}

/// Cloneable bounded sender supplied to one long-lived subscription.
#[derive(Debug)]
pub struct SubscriptionSender<M> {
    sender: mpsc::Sender<Admitted<M>>,
    detailed: bool,
    stats: Arc<Mutex<RuntimeStats>>,
}

impl<M> Clone for SubscriptionSender<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            detailed: self.detailed,
            stats: Arc::clone(&self.stats),
        }
    }
}

impl<M> SubscriptionSender<M> {
    /// Send one subscription message, waiting for bounded capacity.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] after runtime shutdown closes subscription admission.
    pub async fn send(&self, message: M) -> Result<(), SendError> {
        let permit = self.sender.reserve().await.map_err(|_| {
            with_stats(&self.stats, |stats| {
                stats.subscription_rejected = stats.subscription_rejected.saturating_add(1);
            });
            SendError
        })?;
        permit.send(Admitted::new(message, self.detailed));
        let depth = self.sender.max_capacity() - self.sender.capacity();
        with_stats(&self.stats, |stats| {
            stats.subscription_depth = depth;
            stats.subscription_high_water = stats.subscription_high_water.max(depth);
        });
        Ok(())
    }
}

/// Cloneable application and terminal control handle for a running TUI runtime.
#[derive(Debug)]
pub struct RuntimeHandle<M> {
    reliable: ReliableSender<M>,
    terminal: ReliableSender<Event>,
    terminal_latest: LatestSender<Event>,
    latest: LatestSender<M>,
    control: Arc<SharedControl>,
    stats: Arc<Mutex<RuntimeStats>>,
}

impl<M> Clone for RuntimeHandle<M> {
    fn clone(&self) -> Self {
        Self {
            reliable: self.reliable.clone(),
            terminal: self.terminal.clone(),
            terminal_latest: self.terminal_latest.clone(),
            latest: self.latest.clone(),
            control: Arc::clone(&self.control),
            stats: Arc::clone(&self.stats),
        }
    }
}

impl<M> RuntimeHandle<M> {
    /// Send one reliable application message, waiting for bounded capacity.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] after runtime admission closes.
    pub async fn send(&self, message: M) -> Result<(), SendError> {
        self.reliable.send(message).await
    }

    /// Try to send one reliable application message without waiting.
    ///
    /// # Errors
    ///
    /// Returns the original message when admission is full or closed.
    pub fn try_send(&self, message: M) -> Result<(), TrySendError<M>> {
        self.reliable.try_send(message)
    }

    /// Send one terminal event, waiting for the independent bounded input capacity.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] after runtime admission closes.
    pub async fn send_terminal(&self, event: Event) -> Result<(), SendError> {
        self.terminal.send(event).await
    }

    /// Try to send one terminal event without waiting.
    ///
    /// # Errors
    ///
    /// Returns the original event when admission is full or closed.
    pub fn try_send_terminal(&self, event: Event) -> Result<(), TrySendError<Event>> {
        self.terminal.try_send(event)
    }

    /// Coalesce one presentation-safe resize or unpressed mouse-motion event.
    ///
    /// Other terminal events must use reliable admission and are returned unchanged.
    ///
    /// # Errors
    ///
    /// Returns the original event when it is not safe for runtime coalescing, or when keyed
    /// latest-value admission is full or closed.
    pub fn send_latest_terminal(&self, event: Event) -> Result<LatestSendOutcome, Event> {
        let key = match &event {
            Event::Resize(_) => MessageKey::new("terminal.resize"),
            Event::Mouse(mouse) if mouse.kind == bmux_tui::event::MouseEventKind::Move => {
                MessageKey::new("terminal.mouse_motion")
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => return Err(event),
        };
        self.terminal_latest
            .send_latest(key, event)
            .map_err(|error| match error {
                LatestSendError::Full(event) | LatestSendError::Closed(event) => event,
            })
    }

    /// Admit or replace one explicitly replaceable application message.
    ///
    /// # Errors
    ///
    /// Returns the original message when distinct-key admission is full or closed.
    pub fn send_latest(
        &self,
        key: MessageKey,
        message: M,
    ) -> Result<LatestSendOutcome, LatestSendError<M>> {
        self.latest.send_latest(key, message)
    }

    /// Schedule or replace one keyed one-shot timer.
    pub fn schedule_timer(&self, id: TimerId, deadline: Instant) {
        lock_unpoisoned(&self.control.state)
            .timers
            .insert(id, deadline);
        self.control.notify.notify_one();
    }

    /// Cancel one keyed timer, returning whether it was pending.
    #[must_use]
    pub fn cancel_timer(&self, id: &TimerId) -> bool {
        let removed = lock_unpoisoned(&self.control.state)
            .timers
            .remove(id)
            .is_some();
        if removed {
            self.control.notify.notify_one();
        }
        removed
    }

    /// Request a cadence-limited redraw.
    pub fn request_redraw(&self) {
        let coalesced = {
            let mut state = lock_unpoisoned(&self.control.state);
            let coalesced = state.dirty;
            state.dirty = true;
            coalesced
        };
        with_stats(&self.stats, |stats| {
            stats.redraw_requests = stats.redraw_requests.saturating_add(1);
            if coalesced {
                stats.redraw_coalesced = stats.redraw_coalesced.saturating_add(1);
            }
        });
        self.control.notify.notify_one();
    }

    /// Replace the active presentation cadence. `None` disables cadence limiting.
    pub fn set_frame_interval(&self, frame_interval: Option<Duration>) {
        lock_unpoisoned(&self.control.state).frame_interval = frame_interval;
        self.control.notify.notify_one();
    }

    /// Return a point-in-time runtime statistics snapshot.
    #[must_use]
    pub fn stats(&self) -> RuntimeStats {
        stats_snapshot(&self.stats)
    }
}

/// Runtime terminal outcome containing application and presenter ownership.
pub struct RuntimeOutput<P, R> {
    /// Final application state.
    pub program: P,
    /// Presenter after its last successful presentation or failure.
    pub presenter: R,
    /// Final neutral runtime statistics.
    pub stats: RuntimeStats,
}

/// Error returned by the runtime with ownership of final state.
pub enum RuntimeError<PE, RE, P, R> {
    /// Application update failed.
    Program {
        /// Application error.
        error: PE,
        /// Cleanup failure, if protocol ownership could not be restored.
        cleanup_error: Option<RE>,
        /// Final state and measurements.
        output: Box<RuntimeOutput<P, R>>,
    },
    /// Presentation failed.
    Presenter {
        /// Presenter error.
        error: RE,
        /// Additional cleanup failure after the original presentation error.
        cleanup_error: Option<RE>,
        /// Final state and measurements.
        output: Box<RuntimeOutput<P, R>>,
    },
}

// Internal exit reason carries no runtime ownership. Finalization remains in `run`.
enum LoopError<PE, RE> {
    Program(PE),
    Presenter(RE),
}

/// Domain-neutral bounded TUI runtime.
pub struct Runtime<P: Program, R> {
    program: P,
    presenter: R,
    config: RuntimeConfig,
    reliable_rx: mpsc::Receiver<Admitted<P::Message>>,
    terminal_rx: mpsc::Receiver<Admitted<Event>>,
    terminal_latest_rx: LatestReceiver<Event>,
    subscription_rx: mpsc::Receiver<Admitted<P::Message>>,
    latest_rx: LatestReceiver<P::Message>,
    control: Arc<SharedControl>,
    stats: Arc<Mutex<RuntimeStats>>,
    commands: CommandScheduler<P::Message>,
    subscriptions: SubscriptionScheduler<P::Message>,
    last_presented: Option<Instant>,
    pending_presentation: Option<(Instant, Instant)>,
    pending_resize: Option<bmux_tui::geometry::Size>,
    next_ready_source: u8,
    next_application_source: u8,
    exiting: bool,
    constructed_at: Option<Instant>,
    terminal_ordinal: Option<u64>,
    dispatch_window: TerminalDispatchRange,
    presentation_attempt: u64,
    pending_queue_age: Option<(QueueSource, Instant)>,
}

impl<P, R> Runtime<P, R>
where
    P: Program,
    R: Presenter<P>,
{
    /// Create a runtime and its cloneable admission handle.
    #[must_use]
    pub fn new(
        program: P,
        presenter: R,
        config: RuntimeConfig,
    ) -> (Self, RuntimeHandle<P::Message>) {
        let config = config.normalized();
        let stats = Arc::new(Mutex::new(RuntimeStats::new(
            config.collect_detailed_telemetry,
        )));
        let (reliable_tx, reliable_rx) = mpsc::channel(config.reliable_capacity);
        let (terminal_tx, terminal_rx) = mpsc::channel(config.terminal_capacity);
        let (terminal_latest, terminal_latest_rx) = latest_channel(
            2,
            Arc::clone(&stats),
            QueueSource::TerminalLatest,
            config.collect_detailed_telemetry,
        );
        let (subscription_tx, subscription_rx) = mpsc::channel(config.subscription_capacity);
        let (latest, latest_rx) = latest_channel(
            config.latest_capacity,
            Arc::clone(&stats),
            QueueSource::ApplicationLatest,
            config.collect_detailed_telemetry,
        );
        let control = Arc::new(SharedControl {
            state: Mutex::new(ControlState {
                timers: BTreeMap::new(),
                dirty: true,
                reset: false,
                frame_interval: config.frame_interval,
                closed: false,
            }),
            notify: Notify::new(),
        });
        let handle = RuntimeHandle {
            reliable: ReliableSender::new(
                reliable_tx,
                Arc::clone(&stats),
                false,
                config.collect_detailed_telemetry,
            ),
            terminal: ReliableSender::new(
                terminal_tx,
                Arc::clone(&stats),
                true,
                config.collect_detailed_telemetry,
            ),
            terminal_latest,
            latest,
            control: Arc::clone(&control),
            stats: Arc::clone(&stats),
        };
        let commands = CommandScheduler::new(
            config.max_active_commands,
            config.max_queued_commands,
            Arc::clone(&stats),
            config.collect_detailed_telemetry,
        );
        let subscriptions = SubscriptionScheduler::new(
            config.max_active_subscriptions,
            SubscriptionSender {
                sender: subscription_tx,
                detailed: config.collect_detailed_telemetry,
                stats: Arc::clone(&stats),
            },
            Arc::clone(&stats),
        );
        (
            Self {
                program,
                presenter,
                config,
                reliable_rx,
                terminal_rx,
                terminal_latest_rx,
                subscription_rx,
                latest_rx,
                control,
                stats,
                commands,
                subscriptions,
                last_presented: None,
                pending_presentation: None,
                pending_resize: None,
                next_ready_source: 0,
                next_application_source: 0,
                exiting: false,
                constructed_at: config.collect_detailed_telemetry.then(Instant::now),
                terminal_ordinal: Some(0),
                dispatch_window: TerminalDispatchRange::Empty,
                presentation_attempt: 0,
                pending_queue_age: None,
            },
            handle,
        )
    }

    /// Run until the program exits or application/presentation fails.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Program`] for update errors and [`RuntimeError::Presenter`] for
    /// presentation errors. Both variants retain final program and presenter ownership.
    // Errors intentionally return the owned program and presenter for terminal
    // restoration and recovery. Boxing would change this public ownership API.
    #[allow(clippy::result_large_err)]
    pub async fn run(
        mut self,
    ) -> Result<RuntimeOutput<P, R>, RuntimeError<P::Error, R::Error, P, R>> {
        let result = self.run_loop().await;
        self.presenter.cancel_present();
        let restoration = self.presenter.restore().await;
        self.close().await;
        let output = RuntimeOutput {
            program: self.program,
            presenter: self.presenter,
            stats: stats_snapshot(&self.stats),
        };
        match result {
            Ok(()) => match restoration {
                Ok(()) => Ok(output),
                Err(error) => Err(RuntimeError::Presenter {
                    error,
                    cleanup_error: None,
                    output: Box::new(output),
                }),
            },
            Err(LoopError::Program(error)) => Err(RuntimeError::Program {
                error,
                cleanup_error: restoration.err(),
                output: Box::new(output),
            }),
            Err(LoopError::Presenter(error)) => Err(RuntimeError::Presenter {
                error,
                cleanup_error: restoration.err(),
                output: Box::new(output),
            }),
        }
    }

    // Borrow the runtime throughout scheduling. Moving it into cleanup futures at every
    // exit site duplicates large program/presenter temporaries in the generated poll frame.
    async fn run_loop(&mut self) -> Result<(), LoopError<P::Error, R::Error>> {
        loop {
            let did_work = match self.process_ready_turn() {
                Ok(did_work) => did_work,
                Err(error) => return Err(LoopError::Program(error)),
            };

            if self.pending_presentation.is_some() {
                let ready = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(
                        match self.presenter.poll_present(cx, &mut self.program) {
                            std::task::Poll::Ready(result) => Some(result),
                            std::task::Poll::Pending => None,
                        },
                    )
                })
                .await;
                if let Some(result) = ready
                    && let Some((started, scheduled)) = self.pending_presentation.take()
                    && let Err(error) = self.complete_presentation(result, started, scheduled)
                {
                    return Err(LoopError::Presenter(error));
                }
            }
            if self.exiting && self.pending_presentation.is_some() {
                self.presenter.cancel_present();
                self.pending_presentation = None;
                lock_unpoisoned(&self.control.state).dirty = false;
                return Ok(());
            }
            if self.presentation_due()
                && let Err(error) = self.present()
            {
                return Err(LoopError::Presenter(error));
            }
            let dirty = lock_unpoisoned(&self.control.state).dirty;
            if self.exiting && !dirty {
                return Ok(());
            }
            if did_work {
                tokio::task::yield_now().await;
                continue;
            }

            let deadline = self.next_deadline();
            let sleep = async move {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(sleep);

            tokio::select! {
                biased;
                delivery = std::future::poll_fn(|cx| self.presenter.poll_present(cx, &mut self.program)), if self.pending_presentation.is_some() => {
                    if let Some((started, scheduled)) = self.pending_presentation.take()
                        && let Err(error) = self.complete_presentation(delivery, started, scheduled) {
                        return Err(LoopError::Presenter(error));
                    }
                }
                terminal = self.terminal_rx.recv(), if !self.exiting => {
                    if let Some(event) = terminal
                        && let Err(error) = self.apply_admitted(event, QueueSource::TerminalFifo, RuntimeEvent::Terminal)
                    {
                        return Err(LoopError::Program(error));
                    }
                }
                reliable = self.reliable_rx.recv(), if !self.exiting => {
                    if let Some(message) = reliable
                        && let Err(error) = self.apply_admitted(message, QueueSource::ApplicationFifo, RuntimeEvent::Message)
                    {
                        return Err(LoopError::Program(error));
                    }
                }
                completion = self.commands.next_completion(), if !self.exiting && !self.commands.is_empty() => {
                    if let Some(message) = completion.and_then(|completion| completion.message)
                        && let Err(error) = self.apply_event(RuntimeEvent::Message(message))
                    {
                        return Err(LoopError::Program(error));
                    }
                }
                subscription = self.subscription_rx.recv(), if !self.exiting => {
                    if let Some(message) = subscription
                        && let Err(error) = self.apply_admitted(message, QueueSource::Subscription, RuntimeEvent::Message)
                    {
                        return Err(LoopError::Program(error));
                    }
                }
                () = self.subscriptions.next_completion(), if !self.subscriptions.is_empty() => {}
                () = self.terminal_latest_rx.notified() => {}
                () = self.latest_rx.notified() => {}
                () = self.control.notify.notified() => {}
                () = &mut sleep => {}
            }
        }
    }

    fn process_ready_turn(&mut self) -> Result<bool, P::Error> {
        let started = Instant::now();
        let mut processed = 0_usize;
        let mut did_work = false;
        while !self.exiting
            && processed < self.config.messages_per_turn
            && started.elapsed() < self.config.processing_time_per_turn
        {
            let result = self.process_next_ready();
            if result.is_err() && self.config.collect_detailed_telemetry {
                with_stats(&self.stats, |stats| {
                    stats.record_turn(started.elapsed(), self.config.processing_time_per_turn);
                });
            }
            if !result? {
                break;
            }
            processed = processed.saturating_add(1);
            did_work = true;
            if self.exiting {
                break;
            }
        }
        let elapsed = started.elapsed();
        let exhausted = processed == self.config.messages_per_turn
            || elapsed >= self.config.processing_time_per_turn;
        // Under-budget turns add no telemetry lock. Exhausted turns reuse the
        // existing budget-counter lock for the histogram.
        if exhausted {
            with_stats(&self.stats, |stats| {
                stats.scheduler_budget_exhausted =
                    stats.scheduler_budget_exhausted.saturating_add(1);
                stats.record_turn(elapsed, self.config.processing_time_per_turn);
            });
        }
        self.subscriptions.try_next_completion();
        Ok(did_work)
    }

    fn process_next_ready(&mut self) -> Result<bool, P::Error> {
        // Rotate after each serviced source, not after each turn: even a one-message
        // budget must make progress across floods. Each FIFO is still consumed in order.
        const SOURCES: u8 = 6;
        for offset in 0..SOURCES {
            let source = (self.next_ready_source + offset) % SOURCES;
            if self.process_ready_source(source)? {
                self.next_ready_source = (source + 1) % SOURCES;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn process_ready_source(&mut self, source: u8) -> Result<bool, P::Error> {
        let event = match source {
            0 => self.terminal_rx.try_recv().ok().map(|event| {
                observe_receive(&self.terminal_rx, &self.stats, true);
                with_stats(&self.stats, |stats| {
                    stats.terminal_processed = stats.terminal_processed.saturating_add(1);
                });
                RuntimeEvent::Terminal(self.receive(event, QueueSource::TerminalFifo))
            }),
            1 => self.terminal_latest_rx.try_recv().map(|(_key, event)| {
                with_stats(&self.stats, |stats| {
                    stats.terminal_processed = stats.terminal_processed.saturating_add(1);
                });
                RuntimeEvent::Terminal(event)
            }),
            2 => self.take_due_timer(Instant::now()).map(|timer| {
                with_stats(&self.stats, |stats| {
                    stats.timers_delivered = stats.timers_delivered.saturating_add(1);
                });
                RuntimeEvent::Timer(timer)
            }),
            3 => self
                .try_next_application_message()
                .map(RuntimeEvent::Message),
            4 => {
                let Some(completion) = self.commands.try_next_completion() else {
                    return Ok(false);
                };
                if let Some(message) = completion.message {
                    self.apply_event(RuntimeEvent::Message(message))?;
                }
                // A completed command without a message still consumes a service slot.
                return Ok(true);
            }
            _ => self.subscription_rx.try_recv().ok().map(|message| {
                let depth = self.subscription_rx.max_capacity() - self.subscription_rx.capacity();
                with_stats(&self.stats, |stats| stats.subscription_depth = depth);
                RuntimeEvent::Message(self.receive(message, QueueSource::Subscription))
            }),
        };
        let Some(event) = event else {
            return Ok(false);
        };
        self.apply_event(event)?;
        Ok(true)
    }

    fn try_next_application_message(&mut self) -> Option<P::Message> {
        for offset in 0..2 {
            let source = (self.next_application_source + offset) % 2;
            let message = match source {
                0 => self.reliable_rx.try_recv().ok().map(|message| {
                    observe_receive(&self.reliable_rx, &self.stats, false);
                    with_stats(&self.stats, |stats| {
                        stats.reliable_processed = stats.reliable_processed.saturating_add(1);
                    });
                    self.receive(message, QueueSource::ApplicationFifo)
                }),
                _ => self.latest_rx.try_recv().map(|(_key, message)| {
                    with_stats(&self.stats, |stats| {
                        stats.latest_processed = stats.latest_processed.saturating_add(1);
                    });
                    message
                }),
            };
            if message.is_some() {
                self.next_application_source = (source + 1) % 2;
                return message;
            }
        }
        None
    }

    fn receive<T>(&mut self, admitted: Admitted<T>, source: QueueSource) -> T {
        // Scratch state for exactly one dispatch, consumed by apply_event.
        // Reuse its update-start clock and existing accounting lock.
        debug_assert!(self.pending_queue_age.is_none());
        self.pending_queue_age = admitted.at.map(|at| (source, at));
        admitted.value
    }

    fn apply_admitted<T>(
        &mut self,
        admitted: Admitted<T>,
        source: QueueSource,
        event: impl FnOnce(T) -> RuntimeEvent<P::Message>,
    ) -> Result<(), P::Error> {
        let value = self.receive(admitted, source);
        self.apply_event(event(value))
    }

    fn record_terminal_dispatch(&mut self) {
        self.terminal_ordinal = self.terminal_ordinal.and_then(|last| last.checked_add(1));
        self.dispatch_window = match (self.terminal_ordinal, self.dispatch_window) {
            (Some(last), TerminalDispatchRange::Empty) => {
                TerminalDispatchRange::Span { first: last, last }
            }
            (Some(last), TerminalDispatchRange::Span { first, .. }) => {
                TerminalDispatchRange::Span { first, last }
            }
            _ => TerminalDispatchRange::Unknown,
        };
    }

    fn record_presentation(
        &mut self,
        started: Instant,
        ended: Instant,
        succeeded: bool,
        committed: Option<(crate::presenter::PresentReport, Instant)>,
    ) {
        if !self.config.collect_detailed_telemetry && !succeeded {
            return;
        }
        if self.config.collect_detailed_telemetry {
            if let Some(attempt) = self.presentation_attempt.checked_add(1) {
                self.presentation_attempt = attempt;
            } else {
                self.terminal_ordinal = None;
                self.dispatch_window = TerminalDispatchRange::Unknown;
            }
        }
        let terminal_dispatches = self.dispatch_window;
        if self.config.collect_detailed_telemetry {
            self.dispatch_window = if self.terminal_ordinal.is_some() {
                TerminalDispatchRange::Empty
            } else {
                TerminalDispatchRange::Unknown
            };
        }
        with_stats(&self.stats, |stats| {
            let elapsed = ended.saturating_duration_since(started);
            if let Some((report, scheduled_at)) = committed {
                stats.frames_presented = stats.frames_presented.saturating_add(1);
                stats.full_repaints = stats
                    .full_repaints
                    .saturating_add(u64::from(report.full_repaint));
                stats.presented_changed_cells = stats
                    .presented_changed_cells
                    .saturating_add(u64::try_from(report.changed_cells).unwrap_or(u64::MAX));
                stats.presentation_delay_us = stats
                    .presentation_delay_us
                    .saturating_add(micros(ended.saturating_duration_since(scheduled_at)));
                stats.presentation_time_us =
                    stats.presentation_time_us.saturating_add(micros(elapsed));
            }
            if let Some(detailed) = &mut stats.detailed {
                detailed.present_duration.record(elapsed);
                if !succeeded {
                    detailed.presentations_failed = detailed.presentations_failed.saturating_add(1);
                }
                detailed.last_presentation = Some(PresentationSample {
                    attempt: self.presentation_attempt,
                    started_us: micros(
                        started.saturating_duration_since(self.constructed_at.unwrap()),
                    ),
                    elapsed_us: micros(elapsed),
                    succeeded,
                    terminal_dispatches,
                });
            }
        });
    }

    fn apply_event(&mut self, event: RuntimeEvent<P::Message>) -> Result<(), P::Error> {
        if let RuntimeEvent::Terminal(terminal_event) = &event
            && self.presenter.observe_terminal_event(terminal_event)
        {
            self.mark_dirty(false);
        }
        let resize = match &event {
            RuntimeEvent::Terminal(Event::Resize(size)) => Some(*size),
            RuntimeEvent::Terminal(_) | RuntimeEvent::Message(_) | RuntimeEvent::Timer(_) => None,
        };
        if self.config.collect_detailed_telemetry && matches!(&event, RuntimeEvent::Terminal(_)) {
            self.record_terminal_dispatch();
        }
        let update_started = Instant::now();
        let queue_age = self.pending_queue_age.take();
        let result = self.program.update(event);
        if result.is_ok() || self.config.collect_detailed_telemetry {
            let elapsed = update_started.elapsed();
            with_stats(&self.stats, |stats| {
                if let Some((source, admitted_at)) = queue_age {
                    stats.record_queue_age(source, admitted_at, update_started);
                }
                stats.record_update(elapsed, result.is_ok());
            });
        }
        let update = result?;
        if let Some(size) = resize {
            if self.pending_presentation.is_some() {
                self.pending_resize = Some(size);
            } else {
                self.presenter.resize(size);
                self.presenter.reset(ResetReason::Resize);
            }
            let mut state = lock_unpoisoned(&self.control.state);
            state.reset = false;
            state.dirty = true;
        }
        self.apply_update(update);
        Ok(())
    }

    fn apply_update(&mut self, update: Update<P::Message>) {
        match update.invalidation {
            Invalidation::None => {}
            Invalidation::Redraw => self.mark_dirty(false),
            Invalidation::Reset => self.mark_dirty(true),
        }
        for command in update.commands {
            self.commands.schedule(command);
        }
        for key in update.cancelled_subscriptions {
            self.subscriptions.cancel(&key);
        }
        for subscription in update.subscriptions {
            self.subscriptions.replace(subscription);
        }
        match update.lifecycle {
            Lifecycle::Continue => {}
            Lifecycle::Exit => self.exiting = true,
            Lifecycle::Abort => {
                self.exiting = true;
                lock_unpoisoned(&self.control.state).dirty = false;
            }
        }
    }

    fn mark_dirty(&self, reset: bool) {
        let coalesced = {
            let mut state = lock_unpoisoned(&self.control.state);
            let coalesced = state.dirty;
            state.dirty = true;
            state.reset |= reset;
            coalesced
        };
        with_stats(&self.stats, |stats| {
            stats.redraw_requests = stats.redraw_requests.saturating_add(1);
            if coalesced {
                stats.redraw_coalesced = stats.redraw_coalesced.saturating_add(1);
            }
        });
    }

    fn presentation_due(&self) -> bool {
        if self.pending_presentation.is_some() {
            return false;
        }
        let state = lock_unpoisoned(&self.control.state);
        let dirty = state.dirty;
        let frame_interval = state.frame_interval;
        drop(state);
        if !dirty {
            return false;
        }
        let Some(interval) = frame_interval else {
            return true;
        };
        self.last_presented
            .is_none_or(|last| Instant::now() >= last + interval)
    }

    fn present(&mut self) -> Result<(), R::Error> {
        let (reset, scheduled_at) = {
            let mut state = lock_unpoisoned(&self.control.state);
            let scheduled_at = state
                .frame_interval
                .and_then(|interval| self.last_presented.map(|last| last + interval))
                .unwrap_or_else(Instant::now);
            let reset = state.reset;
            // Consume only the invalidation represented by this frame. Requests made while the
            // presenter or successful-commit hook runs remain pending for a later frame.
            state.dirty = false;
            state.reset = false;
            drop(state);
            (reset, scheduled_at)
        };
        if let Some(size) = self.pending_resize.take() {
            self.presenter.resize(size);
            self.presenter.reset(ResetReason::Resize);
        }
        if reset {
            self.presenter.reset(ResetReason::Application);
        }
        let presentation_started = Instant::now();
        match self.presenter.start_present(&mut self.program) {
            Ok(None) => {
                self.pending_presentation = Some((presentation_started, scheduled_at));
                Ok(())
            }
            result => self.complete_presentation(
                result.map(|report| report.expect("immediate report")),
                presentation_started,
                scheduled_at,
            ),
        }
    }

    fn complete_presentation(
        &mut self,
        result: Result<crate::PresentReport, R::Error>,
        presentation_started: Instant,
        scheduled_at: Instant,
    ) -> Result<(), R::Error> {
        let presented_at = Instant::now();
        self.record_presentation(
            presentation_started,
            presented_at,
            result.is_ok(),
            result.as_ref().ok().map(|report| (*report, scheduled_at)),
        );
        let report = result?;
        self.last_presented = Some(presented_at);
        let update = self.program.presentation_committed(report);
        self.apply_update(update);
        Ok(())
    }

    fn take_due_timer(&self, now: Instant) -> Option<TimerId> {
        let due = {
            let state = lock_unpoisoned(&self.control.state);
            let due = state
                .timers
                .iter()
                .filter(|(_, deadline)| **deadline <= now)
                .min_by_key(|(_, deadline)| **deadline)
                .map(|(id, _)| id.clone());
            drop(state);
            due
        }?;
        let deadline = lock_unpoisoned(&self.control.state).timers.remove(&due);
        if self.config.collect_detailed_telemetry
            && let Some(deadline) = deadline
        {
            with_stats(&self.stats, |stats| {
                if let Some(detailed) = &mut stats.detailed {
                    detailed
                        .timer_lateness
                        .record(now.saturating_duration_since(deadline));
                }
            });
        }
        Some(due)
    }

    fn next_deadline(&self) -> Option<Instant> {
        let state = lock_unpoisoned(&self.control.state);
        let timer = if self.exiting {
            None
        } else {
            state.timers.values().min().copied()
        };
        let presentation = if state.dirty && self.pending_presentation.is_none() {
            Some(
                state
                    .frame_interval
                    .and_then(|interval| self.last_presented.map(|last| last + interval))
                    .unwrap_or_else(Instant::now),
            )
        } else {
            None
        };
        [timer, presentation].into_iter().flatten().min()
    }

    async fn close(&mut self) {
        {
            let mut state = lock_unpoisoned(&self.control.state);
            state.closed = true;
            state.timers.clear();
        }
        self.latest_rx.close();
        self.terminal_latest_rx.close();
        self.reliable_rx.close();
        self.terminal_rx.close();
        self.subscription_rx.close();
        self.commands.shutdown().await;
        self.subscriptions.shutdown().await;
    }
}

#[cfg(test)]
mod fairness_tests {
    use std::convert::Infallible;

    use bmux_tui::event::{MouseEvent, MouseEventKind};
    use bmux_tui::geometry::{Point, Size};

    use super::*;
    use crate::{Command, HeadlessPresenter, Subscription, SubscriptionKey};

    #[derive(Default)]
    struct OrderedProgram {
        events: Vec<RuntimeEvent<u64>>,
        movement: i32,
        paused: bool,
        barriers: Vec<i32>,
    }

    impl Program for OrderedProgram {
        type Message = u64;
        type Error = Infallible;

        fn update(&mut self, event: RuntimeEvent<u64>) -> Result<Update<u64>, Infallible> {
            let mut update = Update::none();
            match &event {
                RuntimeEvent::Terminal(Event::Mouse(mouse)) if !self.paused => match mouse.kind {
                    MouseEventKind::ScrollDown => self.movement += 1,
                    MouseEventKind::ScrollUp => self.movement -= 1,
                    _ => {}
                },
                RuntimeEvent::Terminal(Event::User(tag)) if tag == "pause" => {
                    self.barriers.push(self.movement);
                    self.paused = !self.paused;
                }
                RuntimeEvent::Terminal(Event::User(tag)) if tag == "quit" => {
                    self.barriers.push(self.movement);
                    update = Update::exit();
                }
                _ => {}
            }
            self.events.push(event);
            Ok(update)
        }
    }

    fn wheel(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(2, 3)))
    }

    fn one_message_config() -> RuntimeConfig {
        RuntimeConfig {
            messages_per_turn: 1,
            frame_interval: Some(Duration::from_hours(1)),
            processing_time_per_turn: Duration::from_mins(1),
            terminal_capacity: 16,
            ..RuntimeConfig::default()
        }
    }

    #[tokio::test]
    async fn continuous_terminal_input_services_every_ready_source_within_bounded_turns() {
        let (mut runtime, handle) = Runtime::new(
            OrderedProgram::default(),
            HeadlessPresenter::default(),
            one_message_config(),
        );
        // Explicit completion handshakes make readiness independent of wall-clock timing.
        let (command_ready, command_done) = tokio::sync::oneshot::channel();
        runtime.commands.schedule(Command::concurrent(async move {
            command_ready.send(()).expect("test receiver remains open");
            Some(30)
        }));
        let (subscription_ready, subscription_done) = tokio::sync::oneshot::channel();
        runtime.subscriptions.replace(Subscription::new(
            SubscriptionKey::new("resource"),
            |sender| async move {
                sender.send(40).await.expect("runtime remains open");
                subscription_ready
                    .send(())
                    .expect("test receiver remains open");
            },
        ));
        command_done.await.expect("command completed");
        subscription_done.await.expect("resource completion queued");
        handle.try_send(10).expect("reliable capacity");
        handle
            .send_latest(MessageKey::new("progress"), 20)
            .expect("latest capacity");
        handle.schedule_timer(TimerId::new("due"), Instant::now());
        for _ in 0..16 {
            handle
                .try_send_terminal(wheel(MouseEventKind::ScrollDown))
                .expect("terminal capacity");
        }
        for _ in 0..12 {
            handle
                .send_latest_terminal(Event::Resize(Size::new(80, 24)))
                .expect("resize capacity");
            assert!(runtime.process_ready_turn().expect("infallible update"));
            // Refill immediately, without letting the terminal FIFO become empty.
            while handle
                .try_send_terminal(wheel(MouseEventKind::ScrollDown))
                .is_ok()
            {}
        }
        for expected in [10, 20, 30, 40] {
            assert!(
                runtime.program.events.iter().any(|event| {
                    matches!(event, RuntimeEvent::Message(value) if *value == expected)
                }),
                "message {expected} starved behind continuous input"
            );
        }
        assert!(runtime.program.events.iter().any(|event| {
            matches!(event, RuntimeEvent::Timer(timer) if timer.as_str() == "due")
        }));
        assert!(
            runtime
                .program
                .events
                .iter()
                .any(|event| { matches!(event, RuntimeEvent::Terminal(Event::Resize(_))) })
        );
        assert!(runtime.program.movement > 0);
        assert_eq!(runtime.terminal_rx.len(), 16);
        assert_eq!(handle.stats().terminal_high_water, 16);
        runtime.close().await;
    }

    #[tokio::test]
    async fn terminal_fifo_preserves_movement_pause_and_quit_barriers_during_floods() {
        let (mut runtime, handle) = Runtime::new(
            OrderedProgram::default(),
            HeadlessPresenter::default(),
            one_message_config(),
        );
        let input = [
            wheel(MouseEventKind::ScrollDown),
            wheel(MouseEventKind::ScrollDown),
            Event::User("pause".into()),
            wheel(MouseEventKind::ScrollDown),
            Event::User("pause".into()),
            wheel(MouseEventKind::ScrollUp),
            Event::User("quit".into()),
            wheel(MouseEventKind::ScrollDown),
        ];
        for event in &input {
            handle
                .try_send_terminal(event.clone())
                .expect("terminal capacity");
        }
        // Keep both application message classes and timers continuously ready.
        for turn in 0..48 {
            handle
                .try_send(turn)
                .expect("bounded test flood fits reliable capacity");
            handle
                .send_latest(MessageKey::new("progress"), turn)
                .expect("latest capacity");
            handle.schedule_timer(TimerId::new("due"), Instant::now());
            assert!(runtime.process_ready_turn().expect("infallible update"));
            if runtime.exiting {
                break;
            }
        }
        assert!(
            runtime.exiting,
            "quit must not starve behind other ready sources"
        );
        let terminal: Vec<_> = runtime
            .program
            .events
            .iter()
            .filter_map(|event| {
                if let RuntimeEvent::Terminal(event) = event {
                    Some(event.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(terminal, input[..7]);
        assert_eq!(runtime.program.barriers, [2, 2, 1]);
        assert_eq!(runtime.program.movement, 1);
        assert_eq!(
            runtime.terminal_rx.len(),
            1,
            "post-quit input remains unconsumed"
        );
        // Waiting for the final cadence-limited frame must not cross the exit barrier
        // or busy-spin on a timer that the exited program will no longer consume.
        let last = Instant::now();
        runtime.last_presented = Some(last);
        assert!(!runtime.presentation_due());
        assert!(
            !runtime
                .process_ready_turn()
                .expect("exited program is not updated")
        );
        assert_eq!(
            runtime.next_deadline(),
            Some(last + runtime.config.frame_interval.expect("cadence"))
        );
        assert_eq!(runtime.terminal_rx.len(), 1);
        runtime.close().await;
    }
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use crate::{HeadlessPresenter, PresentReport};

    #[derive(Default)]
    struct TestProgram {
        fail: bool,
        commits: usize,
    }

    impl Program for TestProgram {
        type Message = ();
        type Error = &'static str;

        fn update(&mut self, _: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            if self.fail {
                Err("update")
            } else {
                Ok(Update::none())
            }
        }

        fn presentation_committed(&mut self, _: PresentReport) -> Update<()> {
            self.commits += 1;
            Update::none()
        }
    }

    struct FailingPresenter;

    impl Presenter<TestProgram> for FailingPresenter {
        type Error = &'static str;

        fn reset(&mut self, _: ResetReason) {}

        fn present(&mut self, _: &mut TestProgram) -> Result<PresentReport, Self::Error> {
            Err("present")
        }
    }

    #[test]
    fn synthetic_presentation_times_consume_failed_window_without_commit() {
        let (mut runtime, handle) = Runtime::new(
            TestProgram::default(),
            HeadlessPresenter::default(),
            RuntimeConfig {
                collect_detailed_telemetry: true,
                ..RuntimeConfig::default()
            },
        );
        let start = runtime.constructed_at.unwrap() + Duration::from_millis(1);
        runtime.record_terminal_dispatch();
        runtime.record_terminal_dispatch();
        runtime.record_presentation(start, start + Duration::from_micros(501), false, None);
        let stats = handle.stats();
        assert_eq!(
            stats.detailed.as_ref().unwrap().present_duration.buckets[2],
            1
        );
        assert_eq!(stats.detailed.as_ref().unwrap().presentations_failed, 1);
        assert_eq!(stats.frames_presented, 0);
        assert_eq!(runtime.program.commits, 0);
        let sample = stats.detailed.as_ref().unwrap().last_presentation.unwrap();
        assert_eq!(sample.started_us, 1_000);
        assert_eq!(sample.elapsed_us, 501);
        assert!(!sample.succeeded);
        assert_eq!(
            sample.terminal_dispatches,
            TerminalDispatchRange::Span { first: 1, last: 2 }
        );
        runtime.record_presentation(start, start, true, None);
        assert_eq!(
            handle
                .stats()
                .detailed
                .unwrap()
                .last_presentation
                .unwrap()
                .terminal_dispatches,
            TerminalDispatchRange::Empty
        );
        runtime.terminal_ordinal = Some(u64::MAX);
        runtime.record_terminal_dispatch();
        runtime.record_presentation(start, start, true, None);
        assert_eq!(
            handle
                .stats()
                .detailed
                .unwrap()
                .last_presentation
                .unwrap()
                .terminal_dispatches,
            TerminalDispatchRange::Unknown
        );
        runtime.record_presentation(start, start, true, None);
        assert_eq!(
            handle
                .stats()
                .detailed
                .unwrap()
                .last_presentation
                .unwrap()
                .terminal_dispatches,
            TerminalDispatchRange::Unknown
        );
    }

    #[test]
    fn actual_failed_calls_are_recorded_without_committing() {
        let (mut runtime, handle) = Runtime::new(
            TestProgram {
                fail: true,
                commits: 0,
            },
            FailingPresenter,
            RuntimeConfig {
                collect_detailed_telemetry: true,
                ..RuntimeConfig::default()
            },
        );
        assert_eq!(
            runtime.apply_event(RuntimeEvent::Terminal(Event::Tick)),
            Err("update")
        );
        assert_eq!(runtime.present(), Err("present"));
        let stats = handle.stats();
        assert_eq!(stats.detailed.as_ref().unwrap().updates_failed, 1);
        assert_eq!(stats.updates_completed, 0);
        assert_eq!(
            stats
                .detailed
                .as_ref()
                .unwrap()
                .update_duration
                .buckets
                .iter()
                .sum::<u64>(),
            1
        );
        assert_eq!(
            stats
                .detailed
                .as_ref()
                .unwrap()
                .present_duration
                .buckets
                .iter()
                .sum::<u64>(),
            1
        );
        assert_eq!(stats.detailed.as_ref().unwrap().presentations_failed, 1);
        assert_eq!(stats.frames_presented, 0);
        assert_eq!(runtime.program.commits, 0);
        assert!(runtime.last_presented.is_none());
        assert_eq!(
            stats
                .detailed
                .as_ref()
                .unwrap()
                .last_presentation
                .unwrap()
                .terminal_dispatches,
            TerminalDispatchRange::Span { first: 1, last: 1 }
        );
    }

    #[test]
    fn disabled_runtime_preserves_legacy_accounting_without_detailed_state() {
        let (mut runtime, handle) = Runtime::new(
            TestProgram::default(),
            HeadlessPresenter::default(),
            RuntimeConfig::default(),
        );
        assert!(!runtime.config.collect_detailed_telemetry);
        assert!(runtime.constructed_at.is_none());
        handle.try_send_terminal(Event::Tick).unwrap();
        runtime.process_ready_turn().unwrap();
        runtime.present().unwrap();
        let stats = handle.stats();
        assert!(stats.detailed.is_none());
        assert!(stats.queue_age(QueueSource::TerminalFifo).is_none());
        assert_eq!(stats.terminal_processed, 1);
        assert_eq!(stats.updates_completed, 1);
        assert_eq!(stats.frames_presented, 1);
        assert_eq!(runtime.presentation_attempt, 0);
        assert_eq!(runtime.terminal_ordinal, Some(0));
        assert!(runtime.pending_queue_age.is_none());
        assert_eq!(runtime.dispatch_window, TerminalDispatchRange::Empty);
    }

    #[test]
    fn admitted_age_is_batched_once_even_when_update_fails() {
        let (mut runtime, handle) = Runtime::new(
            TestProgram {
                fail: true,
                commits: 0,
            },
            HeadlessPresenter::default(),
            RuntimeConfig {
                collect_detailed_telemetry: true,
                ..RuntimeConfig::default()
            },
        );
        let admitted = Admitted {
            value: (),
            at: Some(Instant::now()),
        };
        assert_eq!(
            runtime.apply_admitted(
                admitted,
                QueueSource::ApplicationFifo,
                RuntimeEvent::Message
            ),
            Err("update")
        );
        assert!(runtime.pending_queue_age.is_none());
        runtime.program.fail = false;
        runtime
            .apply_event(RuntimeEvent::Terminal(Event::Tick))
            .unwrap();
        let stats = handle.stats();
        assert_eq!(
            stats
                .queue_age(QueueSource::ApplicationFifo)
                .unwrap()
                .buckets
                .iter()
                .sum::<u64>(),
            1
        );
        assert_eq!(
            stats
                .queue_age(QueueSource::TerminalFifo)
                .unwrap()
                .buckets
                .iter()
                .sum::<u64>(),
            0
        );
        assert_eq!(stats.detailed.as_ref().unwrap().updates_failed, 1);
        assert_eq!(stats.updates_completed, 1);
    }

    #[test]
    fn synthetic_timer_lateness_is_not_mailbox_age() {
        let (runtime, handle) = Runtime::new(
            TestProgram::default(),
            HeadlessPresenter::default(),
            RuntimeConfig {
                collect_detailed_telemetry: true,
                ..RuntimeConfig::default()
            },
        );
        let deadline = Instant::now();
        let timer = TimerId::new("late");
        handle.schedule_timer(timer.clone(), deadline);
        assert_eq!(
            runtime.take_due_timer(deadline + Duration::from_micros(4_001)),
            Some(timer)
        );
        let stats = handle.stats();
        assert_eq!(
            stats.detailed.as_ref().unwrap().timer_lateness.sum_us,
            4_001
        );
        assert_eq!(
            stats.queue_age(QueueSource::TerminalFifo).unwrap().sum_us,
            0
        );
        assert!(runtime.take_due_timer(deadline).is_none());
    }
}

#[cfg(test)]
#[path = "runtime_exit_tests.rs"]
mod exit_tests;
