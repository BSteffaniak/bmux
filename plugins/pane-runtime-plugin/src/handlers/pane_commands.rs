//! Typed handlers for the `pane-runtime-commands` interface.
//!
//! Each handler decodes its `...Args` wire struct, dispatches through
//! the registered `SessionRuntimeManagerHandle`, and serializes the
//! BPDL result type. When the manager handle is not registered
//! (`session_runtime_handle()` returns `None`), the handler reports a
//! `PaneCommandError::Failed { reason }` / `SessionRuntimeCommandError::Failed`.

use bmux_ipc::{PaneFocusDirection, PaneLaunchCommand, PaneSelector, PaneSplitDirection};
use bmux_pane_runtime_plugin_api::pane_runtime_commands::{
    PaneAck, PaneCommandError, SessionAck, SessionRuntimeCommandError,
};
use bmux_session_models::SessionId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Wire-format argument structs ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub direction: String,
    pub ratio_percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub direction: String,
    pub ratio_percent: u8,
    #[serde(default)]
    pub name: Option<String>,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizePaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub delta_percent: i8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosePaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoomPaneArgs {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneDirectInputArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionArgs {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSessionArgs {
    pub session_id: Uuid,
    pub force_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSessionArgs {
    pub session_id: Uuid,
    pub snapshot_payload: Vec<u8>,
}

// ── Handler bodies ───────────────────────────────────────────────

fn failed_command(reason: impl Into<String>) -> PaneCommandError {
    PaneCommandError::Failed {
        reason: reason.into(),
    }
}

fn failed_session(reason: impl Into<String>) -> SessionRuntimeCommandError {
    SessionRuntimeCommandError::Failed {
        reason: reason.into(),
    }
}

fn parse_split_direction(raw: &str) -> Result<PaneSplitDirection, PaneCommandError> {
    match raw {
        "horizontal" => Ok(PaneSplitDirection::Horizontal),
        "vertical" => Ok(PaneSplitDirection::Vertical),
        other => Err(failed_command(format!(
            "invalid split direction: '{other}' (expected horizontal|vertical)"
        ))),
    }
}

fn parse_focus_direction(raw: &str) -> Result<Option<PaneFocusDirection>, PaneCommandError> {
    match raw {
        "next" => Ok(Some(PaneFocusDirection::Next)),
        "prev" | "previous" => Ok(Some(PaneFocusDirection::Prev)),
        "" => Ok(None),
        other => Err(failed_command(format!(
            "invalid focus direction: '{other}'"
        ))),
    }
}

fn target_selector(target: Option<Uuid>) -> Option<PaneSelector> {
    target.map(PaneSelector::ById)
}

pub fn split_pane(req: &SplitPaneArgs) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let direction = parse_split_direction(&req.direction)?;
    let pane_id = handle
        .0
        .split_pane(
            SessionId(req.session_id),
            target_selector(req.target),
            direction,
        )
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn launch_pane(req: LaunchPaneArgs) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let direction = parse_split_direction(&req.direction)?;
    let command = PaneLaunchCommand {
        program: req.program,
        args: req.args,
        cwd: req.cwd,
        env: std::collections::BTreeMap::new(),
    };
    let pane_id = handle
        .0
        .launch_pane(
            SessionId(req.session_id),
            target_selector(req.target),
            direction,
            req.name,
            command,
        )
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn focus_pane(req: &FocusPaneArgs) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    let pane_id = match (req.target, parse_focus_direction(&req.direction)?) {
        (Some(t), None) => handle
            .0
            .focus_pane_target(session_id, &PaneSelector::ById(t)),
        (None, Some(dir)) => handle.0.focus_pane(session_id, dir),
        (None, None) => handle
            .0
            .focus_pane_target(session_id, &PaneSelector::Active),
        (Some(_), Some(_)) => {
            return Err(failed_command(
                "focus-pane cannot use target and direction together",
            ));
        }
    }
    .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn resize_pane(req: &ResizePaneArgs) -> Result<SessionAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    handle
        .0
        .resize_pane(
            SessionId(req.session_id),
            target_selector(req.target),
            i16::from(req.delta_percent),
        )
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(SessionAck {
        session_id: req.session_id,
    })
}

pub fn close_pane(req: &ClosePaneArgs) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    // Orchestration (session removal cleanup, context pruning,
    // attach-token revocation, event emission) lives in the server's
    // IPC handler until callers migrate to typed dispatch. This
    // handler only performs the pane-runtime portion: invoking
    // close-pane on the manager. When `removed_session` is Some the
    // caller must invoke `shutdown_removed_runtime` etc.
    let (pane_id, _removed) = handle
        .0
        .close_pane(SessionId(req.session_id), target_selector(req.target))
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn restart_pane(req: &RestartPaneArgs) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let pane_id = handle
        .0
        .restart_pane(SessionId(req.session_id), target_selector(req.target))
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn zoom_pane(req: &ZoomPaneArgs) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let (pane_id, _zoomed) = handle
        .0
        .toggle_zoom(SessionId(req.session_id))
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn pane_direct_input(req: PaneDirectInputArgs) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    handle
        .0
        .write_input_to_pane(SessionId(req.session_id), req.pane_id, req.data)
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id: req.pane_id,
    })
}

pub fn new_session_with_runtime(
    req: &NewSessionArgs,
) -> Result<SessionAck, SessionRuntimeCommandError> {
    use bmux_plugin::global_plugin_state_registry;

    let runtime_handle = super::session_runtime_handle()
        .ok_or_else(|| failed_session("pane-runtime manager handle not registered"))?;
    let session_handle = global_plugin_state_registry()
        .get::<bmux_session_state::SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("session manager handle not registered"))?;
    let context_handle = global_plugin_state_registry()
        .get::<bmux_context_state::ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("context state handle not registered"))?;

    // Duplicate-name check mirrors server's create_session_runtime helper.
    if let Some(requested_name) = req.name.as_deref()
        && session_handle
            .0
            .list_sessions()
            .iter()
            .any(|s| s.name.as_deref() == Some(requested_name))
    {
        return Err(SessionRuntimeCommandError::NameAlreadyExists {
            name: requested_name.to_string(),
        });
    }

    let session_id = session_handle
        .0
        .create_session(req.name.clone())
        .map_err(|error| failed_session(format!("failed creating session: {error:#}")))?;

    if let Err(error) = runtime_handle.0.start_runtime(session_id) {
        let _ = session_handle.0.remove_session(session_id);
        return Err(failed_session(format!(
            "failed creating session runtime: {error:#}"
        )));
    }

    // Create+bind a context for the new session. The caller client id
    // is not available in this handler context (the typed invoke
    // context uses `NativeServiceContext` which does carry it, but we
    // accept the caller-less shape here because every code path that
    // reaches `new-session-with-runtime` in production goes through
    // either the sessions-plugin shim or a direct CLI call, both of
    // which pass through a `NativeServiceContext`). When the caller
    // id is absent we fall back to `Uuid::nil` which the context
    // state accepts.
    let caller_client_id = bmux_session_models::ClientId(uuid::Uuid::nil());
    let context = context_handle.0.create(
        caller_client_id,
        req.name.clone(),
        std::collections::BTreeMap::new(),
    );
    if let Err(message) = context_handle.0.bind_session(context.id, session_id) {
        let _ = context_handle
            .0
            .remove_context_by_id(context.id, Some(caller_client_id));
        let _ = session_handle.0.remove_session(session_id);
        if let Some(removed_runtime) = runtime_handle.0.remove_runtime(session_id) {
            runtime_handle.0.shutdown_removed_runtime(removed_runtime);
        }
        return Err(failed_session(format!(
            "failed creating context for new session: {message}"
        )));
    }

    Ok(SessionAck {
        session_id: session_id.0,
    })
}

pub fn kill_session_runtime(
    req: &KillSessionArgs,
) -> Result<SessionAck, SessionRuntimeCommandError> {
    use bmux_plugin::global_plugin_state_registry;

    let runtime_handle = super::session_runtime_handle()
        .ok_or_else(|| failed_session("pane-runtime manager handle not registered"))?;
    let session_handle = global_plugin_state_registry()
        .get::<bmux_session_state::SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("session manager handle not registered"))?;
    let context_handle = global_plugin_state_registry()
        .get::<bmux_context_state::ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("context state handle not registered"))?;
    let attach_token_handle = global_plugin_state_registry()
        .get::<bmux_attach_token_state::AttachTokenManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("attach-token manager handle not registered"))?;
    let follow_handle = global_plugin_state_registry()
        .get::<bmux_client_state::FollowStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("follow state handle not registered"))?;
    let wire_sink = global_plugin_state_registry()
        .get::<bmux_plugin_sdk::WireEventSinkHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()));

    let session_id = SessionId(req.session_id);
    let _ = req.force_local; // admin-principal gate + remote tear-down remain server-side for now.

    if session_handle.0.remove_session(session_id).is_err() {
        return Err(SessionRuntimeCommandError::SessionNotFound);
    }

    let _removed_contexts = context_handle.0.remove_contexts_for_session(session_id);
    follow_handle.0.clear_selections_for_session(session_id);

    let Some(removed_runtime) = runtime_handle.0.remove_runtime(session_id) else {
        return Err(failed_session(format!(
            "failed stopping session runtime: session {} not found",
            session_id.0
        )));
    };

    let had_attached_clients = !removed_runtime.attached_clients.is_empty();
    runtime_handle.0.shutdown_removed_runtime(removed_runtime);
    attach_token_handle.0.remove_for_session(session_id);

    if let Some(sink) = wire_sink {
        if had_attached_clients {
            let _ = sink
                .0
                .publish(bmux_ipc::Event::ClientDetached { id: session_id.0 });
        }
        let _ = sink
            .0
            .publish(bmux_ipc::Event::SessionRemoved { id: session_id.0 });
    }

    Ok(SessionAck {
        session_id: req.session_id,
    })
}

pub fn restore_session_runtime() -> Result<SessionAck, SessionRuntimeCommandError> {
    Err(failed_session(
        "restore-session-runtime is driven by the snapshot orchestrator on startup; \
         clients should not invoke it directly",
    ))
}
