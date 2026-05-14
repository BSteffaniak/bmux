//! Private decoration runtime engine.
//!
//! This module owns the decoration plugin's command/scheduling boundary: theme
//! applies, animation ticks, service mutations, input events, and event-bus
//! subscribers all converge here before touching decoration state. The engine
//! task owns `State` directly; callers interact through command handles and the
//! read model snapshot maintained by the engine.

use std::path::PathBuf;
use std::time::Duration;

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
    spawn_local_current_thread_runtime,
};

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
    AnimationTick(AnimationDriverPolicy),
    PublishInitialScene {
        reply: tokio::sync::oneshot::Sender<()>,
    },
    #[cfg(test)]
    WithState {
        f: Box<dyn FnOnce(&mut State) + Send>,
        reply: tokio::sync::oneshot::Sender<()>,
    },
}

pub(crate) async fn send_engine_command<T>(
    state: &SharedState,
    build: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> DecorationEngineCommand,
) -> Option<T> {
    ensure_decoration_engine(state);
    let tx = state.command_tx()?;
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(build(reply)).ok()?;
    rx.await.ok()
}

pub(crate) fn send_engine_command_blocking<T>(
    state: &SharedState,
    build: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> DecorationEngineCommand,
) -> Option<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return None;
    }
    ensure_decoration_engine(state);
    let tx = state.command_tx()?;
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(build(reply)).ok()?;
    rx.blocking_recv().ok()
}

pub(crate) fn send_engine_fire_and_forget(
    state: &SharedState,
    command: DecorationEngineCommand,
) -> bool {
    ensure_decoration_engine(state);
    state
        .command_tx()
        .and_then(|tx| tx.send(command).ok())
        .is_some()
}

pub(crate) fn ensure_decoration_engine(shared: &SharedState) {
    if shared.command_tx().is_some() {
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    if !shared.set_command_tx(tx) {
        return;
    }
    let shared = shared.clone();
    let host_async_handle = shared.host_async_handle();
    let task = async move {
        let mut state = State::default();
        shared.sync_read_model_from_state(&mut state);
        while let Some(command) = rx.recv().await {
            handle_decoration_engine_command(&shared, &mut state, command);
            shared.sync_read_model_from_state(&mut state);
        }
        tracing::debug!("decoration engine loop exited");
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.engine", task);
    } else {
        spawn_local_current_thread_runtime("decoration engine", task);
    }
}

fn handle_decoration_engine_command(
    shared: &SharedState,
    state: &mut State,
    command: DecorationEngineCommand,
) {
    match command {
        DecorationEngineCommand::SetPaneBorder {
            pane_id,
            border,
            reply,
        } => {
            set_pane_border_direct(state, pane_id, border);
            shared.sync_read_model_from_state(state);
            let _ = reply.send(Ok(()));
        }
        DecorationEngineCommand::SetDefaultBorder { border, reply } => {
            set_default_border_direct(state, border);
            shared.sync_read_model_from_state(state);
            let _ = reply.send(Ok(()));
        }
        DecorationEngineCommand::ApplyThemeExtension {
            toml_text,
            config_dir_candidates,
            script_host_access,
            reply,
        } => {
            let outcome = apply_theme_extension_toml_direct(
                shared,
                state,
                &toml_text,
                &config_dir_candidates,
                &script_host_access,
            );
            shared.sync_read_model_from_state(state);
            let _ = reply.send(outcome);
        }
        DecorationEngineCommand::NotifyPaneEvent { event, reply } => {
            notify_pane_event_direct(state, &event);
            shared.sync_read_model_from_state(state);
            let _ = reply.send(Ok(()));
        }
        DecorationEngineCommand::AttachInput { event, reply } => {
            let result = handle_attach_input_event(state, event);
            shared.sync_read_model_from_state(state);
            let _ = reply.send(result);
        }
        DecorationEngineCommand::PaneEvent(event) => {
            notify_pane_event_direct(state, &event);
        }
        DecorationEngineCommand::FocusSnapshot(snapshot) => {
            apply_focus_state_map(state, &snapshot);
        }
        DecorationEngineCommand::AttachLayoutSnapshot(snapshot) => {
            apply_attach_layout_snapshot(state, &snapshot);
        }
        DecorationEngineCommand::VisualProjection(projection) => {
            apply_visual_projection_batch(state, &projection);
        }
        DecorationEngineCommand::ScriptJsonEvent {
            event,
            snapshot,
            generation,
            reply,
        } => {
            let accepted = enqueue_script_json_event_direct(state, &event, snapshot, generation);
            if let Some(reply) = reply {
                let _ = reply.send(accepted);
            }
        }
        DecorationEngineCommand::AnimationTick(policy) => {
            run_animation_tick_if_current(state, policy);
        }
        DecorationEngineCommand::PublishInitialScene { reply } => {
            publish_scene_if_changed(state);
            shared.sync_read_model_from_state(state);
            let _ = reply.send(());
        }
        #[cfg(test)]
        DecorationEngineCommand::WithState { f, reply } => {
            f(state);
            shared.sync_read_model_from_state(state);
            let _ = reply.send(());
        }
    }
}

pub(crate) fn animation_driver_policy(state: &State) -> AnimationDriverPolicy {
    AnimationDriverPolicy {
        hz: state.animation_hz,
        generation: state.animation_generation,
    }
}

pub(crate) fn notify_animation_driver(shared: &SharedState, state: &State) {
    if let Some(tx) = shared.animation_driver_tx() {
        let _ = tx.send(animation_driver_policy(state));
    }
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

pub(crate) fn ensure_animation_driver(shared: &SharedState) {
    let (tx, rx) = tokio::sync::watch::channel(AnimationDriverPolicy {
        hz: None,
        generation: 0,
    });
    if !shared.set_animation_driver_tx(tx) {
        return;
    }
    let task = animation_driver_loop(shared.clone(), rx);
    if let Some(async_handle) = shared.host_async_handle() {
        async_handle.spawn_with_name("decoration.animation_driver", task);
    } else {
        spawn_local_current_thread_runtime("animation driver", task);
    }
}

async fn animation_driver_loop(
    shared: SharedState,
    mut rx: tokio::sync::watch::Receiver<AnimationDriverPolicy>,
) {
    let mut policy = *rx.borrow();
    loop {
        let Some(hz) = policy.hz.filter(|hz| *hz > 0) else {
            if rx.changed().await.is_err() {
                return;
            }
            policy = *rx.borrow();
            continue;
        };
        let period = Duration::from_micros((1_000_000u64 / u64::from(hz)).max(1));
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return;
                }
                policy = *rx.borrow();
            }
            () = tokio::time::sleep(period) => {
                if !send_engine_fire_and_forget(&shared, DecorationEngineCommand::AnimationTick(policy)) {
                    return;
                }
            }
        }
    }
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
    done_rx
        .blocking_recv()
        .expect("engine state command replied");
    let mut guard = result.lock().expect("result lock");
    guard.take().expect("engine state command produced result")
}
