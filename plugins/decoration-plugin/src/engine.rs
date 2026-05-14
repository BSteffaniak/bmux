//! Private decoration runtime engine.
//!
//! This module owns the decoration plugin's command/scheduling boundary: theme
//! applies, animation ticks, service mutations, input events, and event-bus
//! subscribers all converge here before touching decoration state. The engine
//! runs on a dedicated worker thread and owns `State` directly; callers interact
//! through command handles and the read model snapshot maintained by the engine.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use bmux_decoration_plugin_api::decoration_commands::NotifyError;
use bmux_decoration_plugin_api::decoration_events::PaneEvent;
use bmux_decoration_plugin_api::decoration_state::{BorderStyle, SetStyleError, ValidationResult};
use bmux_plugin::{AttachInputEvent, AttachInputResult};
use uuid::Uuid;

use crate::scripting::ScriptHostAccess;
use crate::{
    SharedState, State, VisualProjectionBatch, apply_attach_layout_snapshot, apply_focus_state_map,
    apply_theme_extension_toml_direct, apply_visual_projection_batch,
    enqueue_script_json_event_direct, handle_attach_input_event, notify_pane_event_direct,
    publish_scene_if_changed, set_default_border_direct, set_pane_border_direct,
};

const ANIMATION_TIMER_FLOOR: Duration = Duration::from_millis(1);
const ENGINE_COMMAND_BUFFER: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnimationDriverPolicy {
    pub(crate) hz: Option<u16>,
    pub(crate) generation: u64,
}

pub(crate) enum DecorationEngineCommand {
    SetPaneBorder {
        pane_id: Uuid,
        border: BorderStyle,
        reply: tokio::sync::oneshot::Sender<Result<(), SetStyleError>>,
    },
    SetDefaultBorder {
        border: BorderStyle,
        reply: tokio::sync::oneshot::Sender<Result<(), SetStyleError>>,
    },
    ApplyThemeExtension {
        toml_text: String,
        config_dir_candidates: Vec<PathBuf>,
        script_host_access: ScriptHostAccess,
        reply: tokio::sync::oneshot::Sender<Result<(), ValidationResult>>,
    },
    NotifyPaneEvent {
        event: PaneEvent,
        reply: tokio::sync::oneshot::Sender<Result<(), NotifyError>>,
    },
    AttachInput {
        event: AttachInputEvent,
        reply: tokio::sync::oneshot::Sender<AttachInputResult>,
    },
    PaneEvent(PaneEvent),
    FocusSnapshot(bmux_pane_runtime_plugin_api::pane_runtime_focus::SessionFocusStateMap),
    AttachLayoutSnapshot(bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot),
    VisualProjection(VisualProjectionBatch),
    ScriptJsonEvent {
        event: bmux_plugin::JsonPluginEvent,
        snapshot: bool,
        generation: u64,
        reply: Option<tokio::sync::oneshot::Sender<bool>>,
    },
    PublishInitialScene {
        reply: tokio::sync::oneshot::Sender<()>,
    },
    #[cfg(test)]
    WithState {
        f: Box<dyn FnOnce(&mut State) + Send>,
        reply: tokio::sync::oneshot::Sender<()>,
    },
}

#[derive(Default)]
struct PendingCoalescedCommands {
    focus: Option<bmux_pane_runtime_plugin_api::pane_runtime_focus::SessionFocusStateMap>,
    layout: Option<bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot>,
    visual_projection: Option<VisualProjectionBatch>,
}

impl PendingCoalescedCommands {
    fn push(&mut self, command: DecorationEngineCommand) -> Option<DecorationEngineCommand> {
        match command {
            DecorationEngineCommand::FocusSnapshot(snapshot) => {
                self.focus = Some(snapshot);
                None
            }
            DecorationEngineCommand::AttachLayoutSnapshot(snapshot) => {
                self.layout = Some(snapshot);
                None
            }
            DecorationEngineCommand::VisualProjection(projection) => {
                self.visual_projection = Some(projection);
                None
            }
            command => Some(command),
        }
    }

    fn flush(&mut self, engine: &mut DecorationEngine) {
        let mut changed = false;
        if let Some(snapshot) = self.focus.take() {
            changed |= apply_focus_state_map(&mut engine.state, &snapshot);
        }
        if let Some(snapshot) = self.layout.take() {
            changed |= apply_attach_layout_snapshot(&mut engine.state, &snapshot);
        }
        if let Some(projection) = self.visual_projection.take() {
            changed |= apply_visual_projection_batch(&mut engine.state, &projection);
        }
        engine.publish_read_model_if(changed);
    }
}

struct DecorationEngine {
    shared: SharedState,
    state: State,
    rx: Receiver<DecorationEngineCommand>,
    next_animation_tick: Option<Instant>,
}

impl DecorationEngine {
    fn new(shared: SharedState, rx: Receiver<DecorationEngineCommand>) -> Self {
        Self {
            shared,
            state: State::default(),
            rx,
            next_animation_tick: None,
        }
    }

    fn run(mut self) {
        self.publish_read_model();
        loop {
            self.update_animation_deadline();
            let received = match self.next_recv_timeout() {
                Some(timeout) => self.rx.recv_timeout(timeout),
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(command) => {
                    self.handle_command(command);
                    self.drain_pending_commands();
                }
                Err(RecvTimeoutError::Timeout) => self.run_animation_tick(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        tracing::debug!("decoration engine loop exited");
    }

    fn next_recv_timeout(&self) -> Option<Duration> {
        self.next_animation_tick
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn update_animation_deadline(&mut self) {
        if !animation_enabled(&self.state) {
            self.next_animation_tick = None;
            return;
        }
        if self.next_animation_tick.is_none() {
            self.next_animation_tick = Some(Instant::now() + animation_period(&self.state));
        }
    }

    fn run_animation_tick(&mut self) {
        let policy = animation_driver_policy(&self.state);
        if run_animation_tick_if_current(&mut self.state, policy) {
            self.publish_read_model();
        }
        self.next_animation_tick =
            animation_enabled(&self.state).then(|| Instant::now() + animation_period(&self.state));
    }

    fn drain_pending_commands(&mut self) {
        let mut pending = PendingCoalescedCommands::default();
        loop {
            match self.rx.try_recv() {
                Ok(command) => {
                    if let Some(command) = pending.push(command) {
                        pending.flush(self);
                        self.handle_command(command);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    pending.flush(self);
                    break;
                }
            }
        }
    }

    fn handle_command(&mut self, command: DecorationEngineCommand) {
        match command {
            DecorationEngineCommand::SetPaneBorder {
                pane_id,
                border,
                reply,
            } => {
                let changed = set_pane_border_direct(&mut self.state, pane_id, border);
                self.publish_read_model_if(changed);
                let _ = reply.send(Ok(()));
            }
            DecorationEngineCommand::SetDefaultBorder { border, reply } => {
                let changed = set_default_border_direct(&mut self.state, border);
                self.publish_read_model_if(changed);
                let _ = reply.send(Ok(()));
            }
            DecorationEngineCommand::ApplyThemeExtension {
                toml_text,
                config_dir_candidates,
                script_host_access,
                reply,
            } => {
                let outcome = apply_theme_extension_toml_direct(
                    &self.shared,
                    &mut self.state,
                    &toml_text,
                    &config_dir_candidates,
                    &script_host_access,
                );
                self.next_animation_tick = None;
                self.publish_read_model();
                let _ = reply.send(outcome);
            }
            DecorationEngineCommand::NotifyPaneEvent { event, reply } => {
                let changed = notify_pane_event_direct(&mut self.state, &event);
                self.publish_read_model_if(changed);
                let _ = reply.send(Ok(()));
            }
            DecorationEngineCommand::AttachInput { event, reply } => {
                let result = handle_attach_input_event(&mut self.state, event);
                self.publish_read_model_if(result.dirty);
                let _ = reply.send(result);
            }
            DecorationEngineCommand::PaneEvent(event) => {
                let changed = notify_pane_event_direct(&mut self.state, &event);
                self.publish_read_model_if(changed);
            }
            DecorationEngineCommand::FocusSnapshot(snapshot) => {
                let changed = apply_focus_state_map(&mut self.state, &snapshot);
                self.publish_read_model_if(changed);
            }
            DecorationEngineCommand::AttachLayoutSnapshot(snapshot) => {
                let changed = apply_attach_layout_snapshot(&mut self.state, &snapshot);
                self.publish_read_model_if(changed);
            }
            DecorationEngineCommand::VisualProjection(projection) => {
                let changed = apply_visual_projection_batch(&mut self.state, &projection);
                self.publish_read_model_if(changed);
            }
            DecorationEngineCommand::ScriptJsonEvent {
                event,
                snapshot,
                generation,
                reply,
            } => {
                let accepted =
                    enqueue_script_json_event_direct(&mut self.state, &event, snapshot, generation);
                self.publish_read_model_if(accepted);
                if let Some(reply) = reply {
                    let _ = reply.send(accepted);
                }
            }
            DecorationEngineCommand::PublishInitialScene { reply } => {
                publish_scene_if_changed(&mut self.state);
                self.publish_read_model();
                let _ = reply.send(());
            }
            #[cfg(test)]
            DecorationEngineCommand::WithState { f, reply } => {
                f(&mut self.state);
                self.next_animation_tick = None;
                self.publish_read_model();
                let _ = reply.send(());
            }
        }
    }

    fn publish_read_model_if(&mut self, changed: bool) {
        if changed {
            self.publish_read_model();
        }
    }

    fn publish_read_model(&mut self) {
        self.shared.sync_read_model_from_state(&mut self.state);
    }
}

pub(crate) async fn send_engine_command<T: Send + 'static>(
    state: &SharedState,
    build: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> DecorationEngineCommand,
) -> Option<T> {
    ensure_decoration_engine(state);
    let tx = state.command_tx()?;
    let (reply, rx) = tokio::sync::oneshot::channel();
    let command = build(reply);
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::spawn_blocking(move || tx.send(command).ok())
            .await
            .ok()
            .flatten()?;
    } else {
        tx.send(command).ok()?;
    }
    rx.await.ok()
}

pub(crate) fn send_engine_command_blocking<T: Send + 'static>(
    state: &SharedState,
    build: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> DecorationEngineCommand,
) -> Option<T> {
    ensure_decoration_engine(state);
    let tx = state.command_tx()?;
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(build(reply)).ok()?;
    recv_engine_reply_blocking(rx)
}

fn recv_engine_reply_blocking<T: Send + 'static>(
    rx: tokio::sync::oneshot::Receiver<T>,
) -> Option<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(move || rx.blocking_recv().ok())
            .join()
            .ok()
            .flatten();
    }
    rx.blocking_recv().ok()
}

pub(crate) fn send_engine_fire_and_forget(
    state: &SharedState,
    command: DecorationEngineCommand,
) -> bool {
    ensure_decoration_engine(state);
    state
        .command_tx()
        .and_then(|tx| tx.try_send(command).ok())
        .is_some()
}

pub(crate) fn ensure_decoration_engine(shared: &SharedState) {
    if shared.command_tx().is_some() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(ENGINE_COMMAND_BUFFER);
    let engine = DecorationEngine::new(shared.clone(), rx);
    let spawn_result = std::thread::Builder::new()
        .name("decoration.engine".to_string())
        .spawn(move || engine.run());
    if spawn_result.is_err() {
        tracing::error!("decoration engine worker failed to spawn");
        return;
    }
    let _ = shared.set_command_tx(tx);
}

pub(crate) fn animation_driver_policy(state: &State) -> AnimationDriverPolicy {
    AnimationDriverPolicy {
        hz: state.animation_hz,
        generation: state.animation_generation,
    }
}

fn animation_enabled(state: &State) -> bool {
    state.animation_hz.is_some_and(|hz| hz > 0) && has_animation_backend(state)
}

fn animation_period(state: &State) -> Duration {
    let hz = state.animation_hz.filter(|hz| *hz > 0).unwrap_or(1);
    Duration::from_micros((1_000_000u64 / u64::from(hz)).max(1)).max(ANIMATION_TIMER_FLOOR)
}

fn has_animation_backend(state: &State) -> bool {
    // Plugin-only client awareness: attach layout geometry is the decoration
    // plugin's current signal that at least one visible surface can consume
    // animation output. True per-client pause/throttle requires attach/client
    // presence semantics that are not part of the decoration inputs yet.
    !state.geometry.is_empty()
        && (state.script_backend.is_some()
            || state
                .script_components
                .values()
                .any(|component| component.backend.is_some()))
}

pub(crate) fn run_animation_tick_if_current(
    state: &mut State,
    policy: AnimationDriverPolicy,
) -> bool {
    if animation_driver_policy(state) != policy || !has_animation_backend(state) {
        return false;
    }
    publish_scene_if_changed(state);
    true
}

#[cfg(test)]
pub(crate) fn with_engine_state<R: Send + 'static>(
    shared: &SharedState,
    f: impl FnOnce(&mut State) -> R + Send + 'static,
) -> R {
    use std::sync::{Arc, Mutex};

    let result = Arc::new(Mutex::new(None));
    let result_for_command = Arc::clone(&result);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let command = DecorationEngineCommand::WithState {
        f: Box::new(move |state| {
            if let Ok(mut guard) = result_for_command.lock() {
                *guard = Some(f(state));
            }
        }),
        reply: done_tx,
    };
    assert!(
        send_engine_fire_and_forget(shared, command),
        "decoration engine is unavailable"
    );
    recv_engine_reply_blocking(done_rx).expect("engine state command replied");
    let mut guard = result.lock().expect("result lock");
    guard.take().expect("engine state command produced result")
}
