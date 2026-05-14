use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bmux_decoration_plugin_api::decoration_commands::NotifyError;
use bmux_decoration_plugin_api::decoration_events::PaneEvent;
use bmux_decoration_plugin_api::decoration_state::{BorderStyle, SetStyleError, ValidationResult};
use bmux_plugin::{AttachInputEvent, AttachInputResult};
use uuid::Uuid;

use crate::scripting::ScriptHostAccess;
use crate::{
    State, VisualProjectionState, apply_attach_layout_snapshot, apply_focus_state_map,
    apply_theme_extension_toml_direct, apply_visual_projection, enqueue_script_json_event_direct,
    handle_attach_input_event, notify_pane_event_direct, publish_scene_if_changed,
    set_default_border_direct, set_pane_border_direct, spawn_local_current_thread_runtime,
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
    VisualProjection(VisualProjectionState),
    ScriptJsonEvent {
        event: bmux_plugin::JsonPluginEvent,
        snapshot: bool,
        generation: u64,
        reply: Option<tokio::sync::oneshot::Sender<bool>>,
    },
    AnimationTick(AnimationDriverPolicy),
}

pub(crate) fn decoration_engine_tx(
    state: &Arc<Mutex<State>>,
) -> Option<tokio::sync::mpsc::UnboundedSender<DecorationEngineCommand>> {
    state.lock().ok().and_then(|guard| guard.engine_tx.clone())
}

pub(crate) async fn send_engine_command<T>(
    state: &Arc<Mutex<State>>,
    build: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> DecorationEngineCommand,
) -> Option<T> {
    let tx = decoration_engine_tx(state)?;
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(build(reply)).ok()?;
    rx.await.ok()
}

pub(crate) fn send_engine_command_blocking<T>(
    state: &Arc<Mutex<State>>,
    build: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> DecorationEngineCommand,
) -> Option<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return None;
    }
    let tx = decoration_engine_tx(state)?;
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(build(reply)).ok()?;
    rx.blocking_recv().ok()
}

pub(crate) fn send_engine_fire_and_forget(
    state: &Arc<Mutex<State>>,
    command: DecorationEngineCommand,
) -> bool {
    decoration_engine_tx(state)
        .and_then(|tx| tx.send(command).ok())
        .is_some()
}

pub(crate) fn ensure_decoration_engine(state: &Arc<Mutex<State>>) {
    let (mut rx, host_async_handle) = {
        let Ok(mut guard) = state.lock() else {
            return;
        };
        if guard.engine_tx.is_some() {
            return;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        guard.engine_tx = Some(tx);
        (rx, guard.host_async_handle.clone())
    };
    let state = Arc::clone(state);
    let task = async move {
        while let Some(command) = rx.recv().await {
            handle_decoration_engine_command(&state, command);
        }
        tracing::debug!("decoration engine loop exited");
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.engine", task);
    } else {
        spawn_local_current_thread_runtime("decoration engine", task);
    }
}

fn handle_decoration_engine_command(state: &Arc<Mutex<State>>, command: DecorationEngineCommand) {
    match command {
        DecorationEngineCommand::SetPaneBorder {
            pane_id,
            border,
            reply,
        } => {
            let _ = reply.send(set_pane_border_direct(state, pane_id, border));
        }
        DecorationEngineCommand::SetDefaultBorder { border, reply } => {
            let _ = reply.send(set_default_border_direct(state, border));
        }
        DecorationEngineCommand::ApplyThemeExtension {
            toml_text,
            config_dir_candidates,
            script_host_access,
            reply,
        } => {
            let outcome = apply_theme_extension_toml_direct(
                state,
                &toml_text,
                &config_dir_candidates,
                script_host_access,
            );
            let _ = reply.send(outcome);
        }
        DecorationEngineCommand::NotifyPaneEvent { event, reply } => {
            let _ = reply.send(notify_pane_event_direct(state, &event));
        }
        DecorationEngineCommand::AttachInput { event, reply } => {
            let result = state.lock().map_or_else(
                |_| AttachInputResult::default(),
                |mut state| handle_attach_input_event(&mut state, event),
            );
            let _ = reply.send(result);
        }
        DecorationEngineCommand::PaneEvent(event) => {
            let _ = notify_pane_event_direct(state, &event);
        }
        DecorationEngineCommand::FocusSnapshot(snapshot) => {
            if let Ok(mut guard) = state.lock() {
                apply_focus_state_map(&mut guard, &snapshot);
            }
        }
        DecorationEngineCommand::AttachLayoutSnapshot(snapshot) => {
            if let Ok(mut guard) = state.lock() {
                apply_attach_layout_snapshot(&mut guard, &snapshot);
            }
        }
        DecorationEngineCommand::VisualProjection(projection) => {
            if let Ok(mut guard) = state.lock() {
                apply_visual_projection(&mut guard, &projection);
            }
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
            if let Ok(mut guard) = state.lock() {
                run_animation_tick_if_current(&mut guard, policy);
            }
        }
    }
}

pub(crate) fn animation_driver_policy(state: &State) -> AnimationDriverPolicy {
    AnimationDriverPolicy {
        hz: state.animation_hz,
        generation: state.animation_generation,
    }
}

pub(crate) fn notify_animation_driver(state: &State) {
    if let Some(tx) = state.animation_driver_tx.as_ref() {
        let _ = tx.send(animation_driver_policy(state));
    }
}

fn has_animation_backend(state: &State) -> bool {
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

pub(crate) fn ensure_animation_driver(state: &Arc<Mutex<State>>) {
    let (rx, host_async_handle) = {
        let Ok(mut guard) = state.lock() else {
            return;
        };
        if guard.animation_driver_tx.is_some() {
            return;
        }
        let (tx, rx) = tokio::sync::watch::channel(animation_driver_policy(&guard));
        guard.animation_driver_tx = Some(tx);
        (rx, guard.host_async_handle.clone())
    };
    let task = animation_driver_loop(Arc::downgrade(state), rx);
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.animation_driver", task);
    } else {
        spawn_local_current_thread_runtime("animation driver", task);
    }
}

async fn animation_driver_loop(
    state: Weak<Mutex<State>>,
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
                let Some(arc) = state.upgrade() else {
                    return;
                };
                let Ok(guard) = arc.lock() else {
                    return;
                };
                if animation_driver_policy(&guard) != policy {
                    policy = animation_driver_policy(&guard);
                    continue;
                }
                drop(guard);
                if !send_engine_fire_and_forget(&arc, DecorationEngineCommand::AnimationTick(policy))
                    && let Ok(mut guard) = arc.lock()
                {
                    run_animation_tick_if_current(&mut guard, policy);
                }
            }
        }
    }
}
