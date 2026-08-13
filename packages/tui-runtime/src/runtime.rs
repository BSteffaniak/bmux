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
use crate::stats::RuntimeStats;
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
    sender: mpsc::Sender<M>,
    stats: Arc<Mutex<RuntimeStats>>,
}

impl<M> Clone for SubscriptionSender<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
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
        self.sender.send(message).await.map_err(|_| {
            with_stats(&self.stats, |stats| {
                stats.subscription_rejected = stats.subscription_rejected.saturating_add(1);
            });
            SendError
        })?;
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
        /// Final state and measurements.
        output: RuntimeOutput<P, R>,
    },
    /// Presentation failed.
    Presenter {
        /// Presenter error.
        error: RE,
        /// Final state and measurements.
        output: RuntimeOutput<P, R>,
    },
}

/// Domain-neutral bounded TUI runtime.
pub struct Runtime<P: Program, R> {
    program: P,
    presenter: R,
    config: RuntimeConfig,
    reliable_rx: mpsc::Receiver<P::Message>,
    terminal_rx: mpsc::Receiver<Event>,
    terminal_latest_rx: LatestReceiver<Event>,
    subscription_rx: mpsc::Receiver<P::Message>,
    latest_rx: LatestReceiver<P::Message>,
    control: Arc<SharedControl>,
    stats: Arc<Mutex<RuntimeStats>>,
    commands: CommandScheduler<P::Message>,
    subscriptions: SubscriptionScheduler<P::Message>,
    last_presented: Option<Instant>,
    next_application_source: u8,
    exiting: bool,
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
        let stats = Arc::new(Mutex::new(RuntimeStats::default()));
        let (reliable_tx, reliable_rx) = mpsc::channel(config.reliable_capacity);
        let (terminal_tx, terminal_rx) = mpsc::channel(config.terminal_capacity);
        let (terminal_latest, terminal_latest_rx) = latest_channel(2, Arc::clone(&stats));
        let (subscription_tx, subscription_rx) = mpsc::channel(config.subscription_capacity);
        let (latest, latest_rx) = latest_channel(config.latest_capacity, Arc::clone(&stats));
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
            reliable: ReliableSender::new(reliable_tx, Arc::clone(&stats), false),
            terminal: ReliableSender::new(terminal_tx, Arc::clone(&stats), true),
            terminal_latest,
            latest,
            control: Arc::clone(&control),
            stats: Arc::clone(&stats),
        };
        let commands = CommandScheduler::new(
            config.max_active_commands,
            config.max_queued_commands,
            Arc::clone(&stats),
        );
        let subscriptions = SubscriptionScheduler::new(
            config.max_active_subscriptions,
            SubscriptionSender {
                sender: subscription_tx,
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
                next_application_source: 0,
                exiting: false,
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
    pub async fn run(
        mut self,
    ) -> Result<RuntimeOutput<P, R>, RuntimeError<P::Error, R::Error, P, R>> {
        loop {
            let did_work = match self.process_ready_turn() {
                Ok(did_work) => did_work,
                Err(error) => return Err(self.program_error(error).await),
            };

            if self.presentation_due()
                && let Err(error) = self.present()
            {
                return Err(self.presenter_error(error).await);
            }
            let dirty = lock_unpoisoned(&self.control.state).dirty;
            if self.exiting && !dirty {
                return Ok(self.finish().await);
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
                terminal = self.terminal_rx.recv() => {
                    if let Some(event) = terminal
                        && let Err(error) = self.apply_event(RuntimeEvent::Terminal(event))
                    {
                        return Err(self.program_error(error).await);
                    }
                }
                reliable = self.reliable_rx.recv() => {
                    if let Some(message) = reliable
                        && let Err(error) = self.apply_event(RuntimeEvent::Message(message))
                    {
                        return Err(self.program_error(error).await);
                    }
                }
                completion = self.commands.next_completion(), if !self.commands.is_empty() => {
                    if let Some(message) = completion.and_then(|completion| completion.message)
                        && let Err(error) = self.apply_event(RuntimeEvent::Message(message))
                    {
                        return Err(self.program_error(error).await);
                    }
                }
                subscription = self.subscription_rx.recv() => {
                    if let Some(message) = subscription
                        && let Err(error) = self.apply_event(RuntimeEvent::Message(message))
                    {
                        return Err(self.program_error(error).await);
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
        while processed < self.config.messages_per_turn
            && started.elapsed() < self.config.processing_time_per_turn
        {
            if let Ok(event) = self.terminal_rx.try_recv() {
                observe_receive(&self.terminal_rx, &self.stats, true);
                with_stats(&self.stats, |stats| {
                    stats.terminal_processed = stats.terminal_processed.saturating_add(1);
                });
                self.apply_event(RuntimeEvent::Terminal(event))?;
            } else if let Some((_key, event)) = self.terminal_latest_rx.try_recv() {
                with_stats(&self.stats, |stats| {
                    stats.terminal_processed = stats.terminal_processed.saturating_add(1);
                });
                self.apply_event(RuntimeEvent::Terminal(event))?;
            } else if let Some(timer) = self.take_due_timer(Instant::now()) {
                with_stats(&self.stats, |stats| {
                    stats.timers_delivered = stats.timers_delivered.saturating_add(1);
                });
                self.apply_event(RuntimeEvent::Timer(timer))?;
            } else if let Some(message) = self.try_next_application_message() {
                self.apply_event(RuntimeEvent::Message(message))?;
            } else if let Some(completion) = self.commands.try_next_completion() {
                if let Some(message) = completion.message {
                    self.apply_event(RuntimeEvent::Message(message))?;
                }
            } else if let Ok(message) = self.subscription_rx.try_recv() {
                let depth = self.subscription_rx.max_capacity() - self.subscription_rx.capacity();
                with_stats(&self.stats, |stats| stats.subscription_depth = depth);
                self.apply_event(RuntimeEvent::Message(message))?;
            } else {
                break;
            }
            processed = processed.saturating_add(1);
            did_work = true;
            if self.exiting {
                break;
            }
        }
        if processed == self.config.messages_per_turn
            || started.elapsed() >= self.config.processing_time_per_turn
        {
            with_stats(&self.stats, |stats| {
                stats.scheduler_budget_exhausted =
                    stats.scheduler_budget_exhausted.saturating_add(1);
            });
        }
        self.subscriptions.try_next_completion();
        Ok(did_work)
    }

    fn try_next_application_message(&mut self) -> Option<P::Message> {
        for offset in 0..2 {
            let source = (self.next_application_source + offset) % 2;
            let message = match source {
                0 => self.reliable_rx.try_recv().ok().inspect(|_| {
                    observe_receive(&self.reliable_rx, &self.stats, false);
                    with_stats(&self.stats, |stats| {
                        stats.reliable_processed = stats.reliable_processed.saturating_add(1);
                    });
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

    fn apply_event(&mut self, event: RuntimeEvent<P::Message>) -> Result<(), P::Error> {
        let resize = match &event {
            RuntimeEvent::Terminal(Event::Resize(size)) => Some(*size),
            RuntimeEvent::Terminal(_) | RuntimeEvent::Message(_) | RuntimeEvent::Timer(_) => None,
        };
        let update_started = Instant::now();
        let update = self.program.update(event)?;
        let update_time_us =
            u64::try_from(update_started.elapsed().as_micros()).unwrap_or(u64::MAX);
        with_stats(&self.stats, |stats| {
            stats.updates_completed = stats.updates_completed.saturating_add(1);
            stats.update_time_us = stats.update_time_us.saturating_add(update_time_us);
        });
        if let Some(size) = resize {
            self.presenter.resize(size);
            self.presenter.reset(ResetReason::Resize);
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
        if reset {
            self.presenter.reset(ResetReason::Application);
        }
        let presentation_started = Instant::now();
        let report = self.presenter.present(&mut self.program)?;
        let presented_at = Instant::now();
        let presentation_time_us = u64::try_from(
            presented_at
                .saturating_duration_since(presentation_started)
                .as_micros(),
        )
        .unwrap_or(u64::MAX);
        self.last_presented = Some(presented_at);
        let delay = presented_at.saturating_duration_since(scheduled_at);
        with_stats(&self.stats, |stats| {
            stats.frames_presented = stats.frames_presented.saturating_add(1);
            stats.full_repaints = stats
                .full_repaints
                .saturating_add(u64::from(report.full_repaint));
            stats.presented_changed_cells = stats
                .presented_changed_cells
                .saturating_add(u64::try_from(report.changed_cells).unwrap_or(u64::MAX));
            stats.presentation_delay_us = stats
                .presentation_delay_us
                .saturating_add(u64::try_from(delay.as_micros()).unwrap_or(u64::MAX));
            stats.presentation_time_us = stats
                .presentation_time_us
                .saturating_add(presentation_time_us);
        });
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
        lock_unpoisoned(&self.control.state).timers.remove(&due);
        Some(due)
    }

    fn next_deadline(&self) -> Option<Instant> {
        let state = lock_unpoisoned(&self.control.state);
        let timer = state.timers.values().min().copied();
        let presentation = if state.dirty {
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

    async fn finish(mut self) -> RuntimeOutput<P, R> {
        self.close().await;
        RuntimeOutput {
            program: self.program,
            presenter: self.presenter,
            stats: stats_snapshot(&self.stats),
        }
    }

    async fn program_error(mut self, error: P::Error) -> RuntimeError<P::Error, R::Error, P, R> {
        self.close().await;
        RuntimeError::Program {
            error,
            output: RuntimeOutput {
                program: self.program,
                presenter: self.presenter,
                stats: stats_snapshot(&self.stats),
            },
        }
    }

    async fn presenter_error(mut self, error: R::Error) -> RuntimeError<P::Error, R::Error, P, R> {
        self.close().await;
        RuntimeError::Presenter {
            error,
            output: RuntimeOutput {
                program: self.program,
                presenter: self.presenter,
                stats: stats_snapshot(&self.stats),
            },
        }
    }
}
