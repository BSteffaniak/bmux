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
    _req: NewSessionArgs,
) -> Result<SessionAck, SessionRuntimeCommandError> {
    // Session creation orchestration (sessions-plugin update +
    // pane-runtime bootstrap + event emission) remains on the server's
    // IPC handler until callers migrate. Returning "not implemented"
    // here signals consumers to stick with `Request::NewSession` for
    // the time being.
    Err(failed_session(
        "new-session-with-runtime is not yet routed through the typed service; \
         callers should continue to use Request::NewSession until the server \
         orchestrator moves into the plugin",
    ))
}

pub fn kill_session_runtime(
    req: &KillSessionArgs,
) -> Result<SessionAck, SessionRuntimeCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_session("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    // Teardown only drives the pane-runtime portion: remove the
    // runtime and schedule async shutdown of PTYs. Context pruning,
    // attach-token revocation, session-manager removal, and event
    // emission remain on the server until that orchestration
    // migrates. When `force_local` is false and remote tear-down is
    // expected, callers still need to use `Request::KillSession`.
    if let Some(removed) = handle.0.remove_runtime(session_id) {
        handle.0.shutdown_removed_runtime(removed);
    }
    let _ = req.force_local;
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
