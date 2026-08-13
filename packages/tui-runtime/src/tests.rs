use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bmux_tui::event::Event;

use crate::{
    Command, CommandKey, HeadlessPresenter, LatestSendOutcome, Lifecycle, MessageKey, Program,
    Runtime, RuntimeConfig, RuntimeEvent, Subscription, SubscriptionKey, TimerId, Update,
};

fn runtime_output<T, E>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|_error| panic!("runtime should succeed"))
}

#[derive(Default)]
struct RecordingProgram {
    messages: Vec<u64>,
    timers: Vec<String>,
    terminal_events: usize,
    exit_after: Option<usize>,
}

impl Program for RecordingProgram {
    type Message = u64;
    type Error = Infallible;

    fn update(&mut self, event: RuntimeEvent<Self::Message>) -> Result<Update<u64>, Self::Error> {
        match event {
            RuntimeEvent::Terminal(_) => self.terminal_events += 1,
            RuntimeEvent::Message(message) => self.messages.push(message),
            RuntimeEvent::Timer(timer) => self.timers.push(timer.as_str().to_owned()),
        }
        let count = self.messages.len() + self.timers.len() + self.terminal_events;
        Ok(if self.exit_after == Some(count) {
            Update {
                lifecycle: Lifecycle::Exit,
                ..Update::redraw()
            }
        } else {
            Update::redraw()
        })
    }
}

fn unlimited_config() -> RuntimeConfig {
    RuntimeConfig {
        frame_interval: None,
        processing_time_per_turn: Duration::from_secs(1),
        ..RuntimeConfig::default()
    }
}

#[tokio::test]
async fn subscription_delivers_through_bounded_runtime_mailbox() {
    struct SubscriptionProgram {
        started: bool,
        messages: Vec<u64>,
    }

    impl Program for SubscriptionProgram {
        type Message = u64;
        type Error = Infallible;

        fn update(
            &mut self,
            event: RuntimeEvent<Self::Message>,
        ) -> Result<Update<u64>, Self::Error> {
            match event {
                RuntimeEvent::Terminal(Event::Tick) if !self.started => {
                    self.started = true;
                    Ok(Update::none().with_subscription(Subscription::new(
                        SubscriptionKey::new("stream"),
                        |sender| async move {
                            sender.send(1).await.expect("runtime remains open");
                            sender.send(2).await.expect("runtime remains open");
                        },
                    )))
                }
                RuntimeEvent::Message(message) => {
                    self.messages.push(message);
                    Ok(if self.messages.len() == 2 {
                        Update::exit()
                    } else {
                        Update::none()
                    })
                }
                RuntimeEvent::Terminal(_) | RuntimeEvent::Timer(_) => Ok(Update::none()),
            }
        }
    }

    let (runtime, handle) = Runtime::new(
        SubscriptionProgram {
            started: false,
            messages: Vec::new(),
        },
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send_terminal(Event::Tick).expect("tick fits");
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.program.messages, [1, 2]);
    assert_eq!(output.stats.subscriptions_started, 1);
    assert_eq!(output.stats.subscriptions_completed, 1);
    assert!(output.stats.subscription_high_water <= unlimited_config().subscription_capacity);
}

#[tokio::test]
async fn replacement_cancels_stale_subscription() {
    struct ReplacingSubscriptionProgram {
        started: bool,
        gate: Arc<tokio::sync::Notify>,
        messages: Vec<u64>,
    }

    impl Program for ReplacingSubscriptionProgram {
        type Message = u64;
        type Error = Infallible;

        fn update(
            &mut self,
            event: RuntimeEvent<Self::Message>,
        ) -> Result<Update<u64>, Self::Error> {
            match event {
                RuntimeEvent::Terminal(Event::Tick) if !self.started => {
                    self.started = true;
                    let gate = Arc::clone(&self.gate);
                    Ok(Update::none()
                        .with_subscription(Subscription::new(
                            SubscriptionKey::new("stream"),
                            move |sender| async move {
                                gate.notified().await;
                                let _result = sender.send(1).await;
                            },
                        ))
                        .with_subscription(Subscription::new(
                            SubscriptionKey::new("stream"),
                            |sender| async move {
                                sender.send(2).await.expect("runtime remains open");
                            },
                        )))
                }
                RuntimeEvent::Message(message) => {
                    self.messages.push(message);
                    Ok(Update::exit())
                }
                RuntimeEvent::Terminal(_) | RuntimeEvent::Timer(_) => Ok(Update::none()),
            }
        }
    }

    let gate = Arc::new(tokio::sync::Notify::new());
    let (runtime, handle) = Runtime::new(
        ReplacingSubscriptionProgram {
            started: false,
            gate: Arc::clone(&gate),
            messages: Vec::new(),
        },
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send_terminal(Event::Tick).expect("tick fits");
    let output = runtime_output(runtime.run().await);
    gate.notify_waiters();
    assert_eq!(output.program.messages, [2]);
    assert!(output.stats.subscriptions_cancelled >= 1);
}

#[tokio::test]
async fn resize_and_mouse_motion_use_explicit_latest_value_admission() {
    use bmux_tui::event::{MouseEvent, MouseEventKind};
    use bmux_tui::geometry::{Point, Size};

    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(2),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    assert_eq!(
        handle
            .send_latest_terminal(Event::Resize(Size::new(80, 24)))
            .expect("resize coalesces"),
        LatestSendOutcome::Inserted
    );
    assert_eq!(
        handle
            .send_latest_terminal(Event::Resize(Size::new(100, 40)))
            .expect("resize replaces"),
        LatestSendOutcome::Replaced
    );
    assert_eq!(
        handle
            .send_latest_terminal(Event::Mouse(MouseEvent::new(
                MouseEventKind::Move,
                Point::new(2, 3),
            )))
            .expect("motion coalesces"),
        LatestSendOutcome::Inserted
    );
    assert!(handle.send_latest_terminal(Event::Tick).is_err());
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.program.terminal_events, 2);
    assert_eq!(output.stats.latest_replaced, 1);
    assert_eq!(output.stats.terminal_processed, 2);
}

#[tokio::test]
async fn disabled_cadence_presents_dirty_state_promptly() {
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(1),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        RuntimeConfig {
            frame_interval: None,
            ..RuntimeConfig::default()
        },
    );
    handle.try_send(1).expect("message fits");
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.stats.frames_presented, 1);
}

struct ReportingPresenter {
    report: crate::PresentReport,
}

impl<P> crate::Presenter<P> for ReportingPresenter {
    type Error = Infallible;

    fn reset(&mut self, _reason: crate::ResetReason) {}

    fn present(&mut self, _program: &mut P) -> Result<crate::PresentReport, Self::Error> {
        Ok(self.report)
    }
}

#[tokio::test]
async fn successful_updates_and_presenter_reports_are_accounted() {
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(1),
            ..RecordingProgram::default()
        },
        ReportingPresenter {
            report: crate::PresentReport {
                changed_cells: 7,
                full_repaint: true,
            },
        },
        unlimited_config(),
    );
    handle.try_send(1).expect("message fits");

    let output = runtime_output(runtime.run().await);

    assert_eq!(output.stats.updates_completed, 1);
    assert_eq!(output.stats.frames_presented, 1);
    assert_eq!(output.stats.full_repaints, 1);
    assert_eq!(output.stats.presented_changed_cells, 7);
}

#[tokio::test]
async fn no_op_commit_hook_does_not_add_a_frame() {
    struct ExitAfterMessage;

    impl Program for ExitAfterMessage {
        type Message = ();
        type Error = Infallible;

        fn update(&mut self, event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(if matches!(event, RuntimeEvent::Message(())) {
                Update::redraw().merge(Update::exit())
            } else {
                Update::none()
            })
        }
    }

    let (runtime, handle) = Runtime::new(
        ExitAfterMessage,
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send(()).expect("message fits");
    let output = runtime_output(runtime.run().await);

    assert_eq!(output.presenter.presentations(), 1);
    assert_eq!(output.stats.frames_presented, 1);
}

#[tokio::test]
async fn successful_presentation_commit_can_schedule_a_follow_up_frame() {
    #[derive(Default)]
    struct CommitProgram {
        commits: usize,
    }

    impl Program for CommitProgram {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.commits += 1;
            if self.commits == 1 {
                Update::redraw()
            } else {
                Update::exit()
            }
        }

        fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(Update::none())
        }
    }

    let (runtime, _handle) = Runtime::new(
        CommitProgram::default(),
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    let output = runtime_output(runtime.run().await);

    assert_eq!(output.program.commits, 2);
    assert_eq!(output.presenter.presentations(), 2);
    assert_eq!(output.stats.frames_presented, 2);
}

#[tokio::test]
async fn successful_presentation_commit_follow_up_obeys_cadence_and_counts_each_frame() {
    #[derive(Default)]
    struct CommitProgram {
        commits: usize,
    }

    impl Program for CommitProgram {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.commits += 1;
            if self.commits == 1 {
                Update::redraw()
            } else {
                Update::exit()
            }
        }

        fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(Update::none())
        }
    }

    struct TimestampPresenter(Vec<Instant>);

    impl<P> crate::Presenter<P> for TimestampPresenter {
        type Error = Infallible;

        fn reset(&mut self, _reason: crate::ResetReason) {}

        fn present(&mut self, _program: &mut P) -> Result<crate::PresentReport, Self::Error> {
            self.0.push(Instant::now());
            Ok(crate::PresentReport {
                changed_cells: 3,
                full_repaint: false,
            })
        }
    }

    let frame_interval = Duration::from_millis(20);
    let (runtime, _handle) = Runtime::new(
        CommitProgram::default(),
        TimestampPresenter(Vec::new()),
        RuntimeConfig {
            frame_interval: Some(frame_interval),
            processing_time_per_turn: Duration::from_secs(1),
            ..RuntimeConfig::default()
        },
    );
    let output = runtime_output(runtime.run().await);

    assert_eq!(output.program.commits, 2);
    assert_eq!(output.presenter.0.len(), 2);
    assert!(output.presenter.0[1].duration_since(output.presenter.0[0]) >= frame_interval);
    assert_eq!(output.stats.frames_presented, 2);
    assert_eq!(output.stats.presented_changed_cells, 6);
    assert_eq!(output.stats.full_repaints, 0);
}

#[tokio::test]
async fn successful_presentation_commit_can_schedule_a_follow_up_reset() {
    #[derive(Default)]
    struct CommitProgram {
        commits: usize,
    }

    impl Program for CommitProgram {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.commits += 1;
            if self.commits == 1 {
                Update::reset()
            } else {
                Update::exit()
            }
        }

        fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(Update::none())
        }
    }

    let (runtime, _handle) = Runtime::new(
        CommitProgram::default(),
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    let output = runtime_output(runtime.run().await);

    assert_eq!(output.program.commits, 2);
    assert_eq!(output.presenter.presentations(), 2);
    assert_eq!(output.presenter.resets(), 1);
}

struct RedrawingPresenter {
    handle: Arc<Mutex<Option<crate::RuntimeHandle<()>>>>,
    presentations: usize,
}

impl<P> crate::Presenter<P> for RedrawingPresenter {
    type Error = Infallible;

    fn reset(&mut self, _reason: crate::ResetReason) {}

    fn present(&mut self, _program: &mut P) -> Result<crate::PresentReport, Self::Error> {
        self.presentations += 1;
        if self.presentations == 1 {
            self.handle
                .lock()
                .expect("handle lock")
                .as_ref()
                .expect("runtime handle installed")
                .request_redraw();
        }
        Ok(crate::PresentReport::default())
    }
}

#[tokio::test]
async fn commit_requested_reset_dominates_redraw_requested_during_presentation() {
    struct ResetThenExit(usize);

    impl Program for ResetThenExit {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.0 += 1;
            if self.0 == 1 {
                Update::reset()
            } else {
                Update::exit()
            }
        }

        fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(Update::none())
        }
    }

    struct RedrawOncePresenter {
        handle: Arc<Mutex<Option<crate::RuntimeHandle<()>>>>,
        presentations: usize,
        resets: usize,
    }

    impl crate::Presenter<ResetThenExit> for RedrawOncePresenter {
        type Error = Infallible;

        fn reset(&mut self, _reason: crate::ResetReason) {
            self.resets += 1;
        }

        fn present(
            &mut self,
            _program: &mut ResetThenExit,
        ) -> Result<crate::PresentReport, Self::Error> {
            self.presentations += 1;
            if self.presentations == 1 {
                self.handle
                    .lock()
                    .expect("handle lock")
                    .as_ref()
                    .expect("runtime handle installed")
                    .request_redraw();
            }
            Ok(crate::PresentReport::default())
        }
    }

    let handle_slot = Arc::new(Mutex::new(None));
    let (runtime, handle) = Runtime::new(
        ResetThenExit(0),
        RedrawOncePresenter {
            handle: Arc::clone(&handle_slot),
            presentations: 0,
            resets: 0,
        },
        unlimited_config(),
    );
    *handle_slot.lock().expect("handle lock") = Some(handle);
    let output = runtime_output(runtime.run().await);

    assert_eq!(output.presenter.presentations, 2);
    assert_eq!(output.presenter.resets, 1);
}

#[tokio::test]
async fn graceful_exit_waits_for_commit_requested_follow_up_frame() {
    struct ExitProgram(usize);

    impl Program for ExitProgram {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.0 += 1;
            if self.0 == 1 {
                Update::redraw()
            } else {
                Update::none()
            }
        }

        fn update(&mut self, event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(if matches!(event, RuntimeEvent::Message(())) {
                Update::redraw().merge(Update::exit())
            } else {
                Update::none()
            })
        }
    }

    let (runtime, handle) = Runtime::new(
        ExitProgram(0),
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send(()).expect("message fits");
    let output = runtime_output(runtime.run().await);

    assert_eq!(output.program.0, 2);
    assert_eq!(output.presenter.presentations(), 2);
    assert_eq!(output.stats.frames_presented, 2);
}

#[tokio::test]
async fn redraw_requested_during_presentation_survives_the_commit() {
    struct ExitAfterSecondCommit(usize);

    impl Program for ExitAfterSecondCommit {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.0 += 1;
            if self.0 == 2 {
                Update::exit()
            } else {
                Update::none()
            }
        }

        fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(Update::none())
        }
    }

    let handle_slot = Arc::new(Mutex::new(None));
    let (runtime, handle) = Runtime::new(
        ExitAfterSecondCommit(0),
        RedrawingPresenter {
            handle: Arc::clone(&handle_slot),
            presentations: 0,
        },
        unlimited_config(),
    );
    *handle_slot.lock().expect("handle lock") = Some(handle);
    let output = runtime_output(runtime.run().await);

    assert_eq!(output.program.0, 2);
    assert_eq!(output.presenter.presentations, 2);
}

#[tokio::test]
async fn finite_commit_redraw_chain_becomes_idle_after_exact_frames() {
    struct FiniteProgram(usize);

    impl Program for FiniteProgram {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.0 += 1;
            if self.0 < 4 {
                Update::redraw()
            } else {
                Update::exit()
            }
        }

        fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(Update::none())
        }
    }

    let (runtime, _handle) = Runtime::new(
        FiniteProgram(0),
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    let output = tokio::time::timeout(Duration::from_secs(1), runtime.run())
        .await
        .expect("finite redraw chain becomes idle and exits");
    let output = runtime_output(output);

    assert_eq!(output.program.0, 4);
    assert_eq!(output.presenter.presentations(), 4);
    assert_eq!(output.stats.frames_presented, 4);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PresentFailure;

struct FailingPresenter;

impl<P> crate::Presenter<P> for FailingPresenter {
    type Error = PresentFailure;

    fn reset(&mut self, _reason: crate::ResetReason) {}

    fn present(&mut self, _program: &mut P) -> Result<crate::PresentReport, Self::Error> {
        Err(PresentFailure)
    }
}

#[tokio::test]
async fn presenter_failure_is_terminal_and_does_not_commit_frame() {
    struct CommitTrackingProgram {
        commits: usize,
    }

    impl Program for CommitTrackingProgram {
        type Message = ();
        type Error = Infallible;

        fn presentation_committed(&mut self, _report: crate::PresentReport) -> Update<()> {
            self.commits += 1;
            Update::none()
        }

        fn update(&mut self, _event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(Update::none())
        }
    }

    let (runtime, _handle) = Runtime::new(
        CommitTrackingProgram { commits: 0 },
        FailingPresenter,
        unlimited_config(),
    );
    let error = runtime.run().await.err().expect("presenter fails");
    let crate::RuntimeError::Presenter { error, output } = error;
    assert_eq!(error, PresentFailure);
    assert_eq!(output.program.commits, 0);
    assert_eq!(output.stats.frames_presented, 0);
    assert_eq!(output.stats.full_repaints, 0);
    assert_eq!(output.stats.presented_changed_cells, 0);
    assert_eq!(output.stats.presentation_time_us, 0);
}

#[tokio::test]
async fn cadence_reload_wakes_pending_dirty_presentation() {
    let config = RuntimeConfig {
        frame_interval: Some(Duration::from_mins(1)),
        ..RuntimeConfig::default()
    };
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(1),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        config,
    );
    handle.set_frame_interval(None);
    handle.try_send(1).expect("message fits");
    let output = tokio::time::timeout(Duration::from_secs(1), runtime.run())
        .await
        .expect("reload prevents long cadence wait");
    assert_eq!(runtime_output(output).stats.frames_presented, 1);
}

#[tokio::test]
async fn reset_invalidation_resets_presenter_before_committing_frame() {
    struct ResetProgram;

    impl Program for ResetProgram {
        type Message = ();
        type Error = Infallible;

        fn update(&mut self, event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(if matches!(event, RuntimeEvent::Message(())) {
                Update {
                    lifecycle: Lifecycle::Exit,
                    ..Update::reset()
                }
            } else {
                Update::none()
            })
        }
    }

    let (runtime, handle) = Runtime::new(
        ResetProgram,
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send(()).expect("message fits");
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.presenter.resets(), 1);
    assert_eq!(output.presenter.presentations(), 1);
    assert_eq!(output.stats.frames_presented, 1);
}

#[tokio::test]
async fn shutdown_aborts_runtime_owned_command_tasks() {
    struct DropSignal(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct ShutdownProgram {
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Program for ShutdownProgram {
        type Message = ();
        type Error = Infallible;

        fn update(&mut self, event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
            Ok(if matches!(event, RuntimeEvent::Terminal(Event::Tick)) {
                let signal = DropSignal(Arc::clone(&self.dropped));
                Update {
                    lifecycle: Lifecycle::Exit,
                    ..Update::none().with_command(Command::concurrent(async move {
                        let _signal = signal;
                        std::future::pending::<Option<()>>().await
                    }))
                }
            } else {
                Update::none()
            })
        }
    }

    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (runtime, handle) = Runtime::new(
        ShutdownProgram {
            dropped: Arc::clone(&dropped),
        },
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send_terminal(Event::Tick).expect("tick fits");
    let output = runtime_output(runtime.run().await);
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(output.stats.commands_cancelled, 1);
}

#[tokio::test]
async fn reliable_messages_remain_ordered_and_lossless() {
    let program = RecordingProgram {
        exit_after: Some(10_000),
        ..RecordingProgram::default()
    };
    let (runtime, handle) = Runtime::new(program, HeadlessPresenter::default(), unlimited_config());
    let sender = tokio::spawn(async move {
        for value in 0..10_000 {
            handle.send(value).await.expect("runtime remains open");
        }
    });
    let output = runtime_output(runtime.run().await);
    sender.await.expect("sender succeeds");
    assert_eq!(output.program.messages, (0..10_000).collect::<Vec<_>>());
    assert!(output.stats.reliable_high_water <= unlimited_config().reliable_capacity);
}

#[tokio::test]
async fn reliable_sender_waits_for_capacity() {
    let config = RuntimeConfig {
        reliable_capacity: 1,
        frame_interval: None,
        ..RuntimeConfig::default()
    };
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(2),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        config,
    );
    handle.try_send(1).expect("first message fits");
    assert!(matches!(
        handle.try_send(2),
        Err(crate::TrySendError::Full(2))
    ));
    let sender = handle.clone();
    let blocked = tokio::spawn(async move { sender.send(2).await });
    tokio::task::yield_now().await;
    assert!(!blocked.is_finished());
    let output = runtime_output(runtime.run().await);
    blocked
        .await
        .expect("sender task succeeds")
        .expect("second message admitted");
    assert_eq!(output.program.messages, [1, 2]);
    assert_eq!(output.stats.reliable_rejected, 1);
}

#[tokio::test]
async fn latest_value_flood_occupies_one_key_and_delivers_latest() {
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(1),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    for value in 0..100_000 {
        let outcome = handle
            .send_latest(MessageKey::new("progress"), value)
            .expect("same key remains admissible");
        assert_eq!(
            outcome,
            if value == 0 {
                LatestSendOutcome::Inserted
            } else {
                LatestSendOutcome::Replaced
            }
        );
    }
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.program.messages, [99_999]);
    assert_eq!(output.stats.latest_high_water, 1);
    assert_eq!(output.stats.latest_replaced, 99_999);
}

#[tokio::test]
async fn terminal_input_is_serviced_during_application_flood() {
    let config = RuntimeConfig {
        reliable_capacity: 20_000,
        messages_per_turn: 4,
        frame_interval: None,
        ..RuntimeConfig::default()
    };
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(10_001),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        config,
    );
    for value in 0..10_000 {
        handle.try_send(value).expect("flood fits configured bound");
    }
    handle
        .try_send_terminal(Event::Tick)
        .expect("terminal has independent capacity");
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.program.terminal_events, 1);
    assert_eq!(output.program.messages.len(), 10_000);
    assert!(output.stats.scheduler_budget_exhausted > 0);
}

#[tokio::test]
async fn due_timer_is_serviced_during_application_flood() {
    let config = RuntimeConfig {
        reliable_capacity: 20_000,
        messages_per_turn: 4,
        frame_interval: None,
        ..RuntimeConfig::default()
    };
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(10_001),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        config,
    );
    for value in 0..10_000 {
        handle.try_send(value).expect("flood fits configured bound");
    }
    handle.schedule_timer(TimerId::new("due"), Instant::now());
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.program.timers, ["due"]);
    assert_eq!(output.program.messages.len(), 10_000);
    assert!(output.stats.scheduler_budget_exhausted > 0);
}

#[tokio::test]
async fn keyed_timer_replacement_delivers_once() {
    let (runtime, handle) = Runtime::new(
        RecordingProgram {
            exit_after: Some(1),
            ..RecordingProgram::default()
        },
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    let timer = TimerId::new("animation");
    handle.schedule_timer(timer.clone(), Instant::now() + Duration::from_secs(1));
    handle.schedule_timer(timer, Instant::now());
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.program.timers, ["animation"]);
    assert_eq!(output.stats.timers_delivered, 1);
}

struct CommandProgram {
    messages: Vec<u64>,
    started: bool,
    gate: Arc<tokio::sync::Notify>,
}

impl Program for CommandProgram {
    type Message = u64;
    type Error = Infallible;

    fn update(&mut self, event: RuntimeEvent<u64>) -> Result<Update<u64>, Self::Error> {
        match event {
            RuntimeEvent::Terminal(Event::Tick) if !self.started => {
                self.started = true;
                let first_gate = Arc::clone(&self.gate);
                Ok(Update::none()
                    .with_command(Command::replace(CommandKey::new("work"), async move {
                        first_gate.notified().await;
                        Some(1)
                    }))
                    .with_command(Command::replace(CommandKey::new("work"), async { Some(2) })))
            }
            RuntimeEvent::Message(message) => {
                self.messages.push(message);
                Ok(Update::exit())
            }
            RuntimeEvent::Terminal(_) | RuntimeEvent::Timer(_) => Ok(Update::none()),
        }
    }
}

#[tokio::test]
async fn replacement_suppresses_stale_command_completion() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (runtime, handle) = Runtime::new(
        CommandProgram {
            messages: Vec::new(),
            started: false,
            gate: Arc::clone(&gate),
        },
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send_terminal(Event::Tick).expect("tick fits");
    let output = runtime_output(runtime.run().await);
    gate.notify_waiters();
    assert_eq!(output.program.messages, [2]);
    assert!(output.stats.commands_cancelled >= 1);
}

struct CadenceProgram {
    updates: usize,
}

impl Program for CadenceProgram {
    type Message = ();
    type Error = Infallible;

    fn update(&mut self, event: RuntimeEvent<()>) -> Result<Update<()>, Self::Error> {
        if matches!(event, RuntimeEvent::Message(())) {
            self.updates += 1;
        }
        Ok(if self.updates == 100 {
            Update {
                lifecycle: Lifecycle::Exit,
                ..Update::redraw()
            }
        } else {
            Update::redraw()
        })
    }
}

#[tokio::test]
async fn redraw_flood_is_coalesced_by_cadence() {
    let config = RuntimeConfig {
        reliable_capacity: 128,
        frame_interval: Some(Duration::from_millis(10)),
        ..RuntimeConfig::default()
    };
    let (runtime, handle) = Runtime::new(
        CadenceProgram { updates: 0 },
        HeadlessPresenter::default(),
        config,
    );
    for () in [(); 100] {
        handle.try_send(()).expect("configured queue fits flood");
    }
    let output = runtime_output(runtime.run().await);
    assert_eq!(output.program.updates, 100);
    assert!(output.stats.frames_presented <= 2);
    assert!(output.stats.redraw_coalesced > 0);
}

#[test]
fn statistics_snapshot_is_safe_during_admission() {
    let (_runtime, handle) = Runtime::new(
        RecordingProgram::default(),
        HeadlessPresenter::default(),
        unlimited_config(),
    );
    handle.try_send(1).expect("message fits");
    let stats = Arc::new(Mutex::new(handle.stats()));
    assert_eq!(stats.lock().expect("lock").reliable_depth, 1);
}
