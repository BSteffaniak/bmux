//! Typed handlers for the `pane-runtime-state` interface.

use bmux_client_state::FollowStateHandle;
use bmux_pane_runtime_plugin_api::pane_runtime_state::{
    FloatingPaneList, FloatingPaneSummary, PaneProcessIdentity, PaneProcessList, PaneStateError,
    PaneSummary, SessionPaneList,
};
use bmux_pane_runtime_state::{FloatingPaneLayer, FloatingPaneScope};
use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::NativeServiceContext;
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPanesArgs {
    /// Explicit session id to list. When absent the handler resolves
    /// the caller's currently-selected session via `FollowState`.
    #[serde(default)]
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPaneArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPaneProcessArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListFloatingPanesArgs {
    pub session_id: Uuid,
}

const fn floating_scope_name(scope: FloatingPaneScope) -> &'static str {
    match scope {
        FloatingPaneScope::PerPane => "per_pane",
        FloatingPaneScope::PerWindow => "per_window",
        FloatingPaneScope::PerSession => "per_session",
        FloatingPaneScope::ClientGlobal => "client_global",
        FloatingPaneScope::ServerGlobal => "server_global",
    }
}

const fn floating_layer_name(layer: FloatingPaneLayer) -> &'static str {
    match layer {
        FloatingPaneLayer::Pane => "pane",
        FloatingPaneLayer::Overlay => "overlay",
        FloatingPaneLayer::FloatingPane => "floating_pane",
        FloatingPaneLayer::Tooltip => "tooltip",
    }
}

/// Resolve a caller-optional session id to a concrete session.
fn resolve_session_id(
    explicit: Option<Uuid>,
    caller_client_id: Option<Uuid>,
) -> Result<SessionId, PaneStateError> {
    if let Some(id) = explicit {
        return Ok(SessionId(id));
    }
    let client_id = caller_client_id.ok_or(PaneStateError::SessionNotFound)?;
    let follow = global_plugin_state_registry()
        .get::<FollowStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or(PaneStateError::SessionNotFound)?;
    follow
        .0
        .selected_session(ClientId(client_id))
        .ok_or(PaneStateError::SessionNotFound)
}

pub fn list_panes(
    req: &ListPanesArgs,
    ctx: &NativeServiceContext,
) -> Result<SessionPaneList, PaneStateError> {
    let handle = super::session_runtime_handle().ok_or(PaneStateError::SessionNotFound)?;
    let session_id = resolve_session_id(req.session_id, ctx.caller_client_id)?;
    let summaries = handle
        .0
        .list_panes(session_id)
        .map_err(|_| PaneStateError::SessionNotFound)?;
    Ok(SessionPaneList {
        session_id: session_id.0,
        panes: summaries
            .into_iter()
            .map(|p| PaneSummary {
                id: p.id,
                name: p.name,
                // `shell` is part of the BPDL contract but the IPC
                // summary doesn't carry it; callers can query the
                // session runtime for process details when needed.
                shell: String::new(),
                focused: p.focused,
            })
            .collect(),
    })
}

pub fn list_floating_panes(
    req: &ListFloatingPanesArgs,
) -> Result<FloatingPaneList, PaneStateError> {
    let handle = super::session_runtime_handle().ok_or(PaneStateError::SessionNotFound)?;
    let session_id = SessionId(req.session_id);
    let panes = handle
        .0
        .list_floating_panes(session_id)
        .map_err(|_| PaneStateError::SessionNotFound)?
        .into_iter()
        .map(|surface| FloatingPaneSummary {
            id: surface.id,
            pane_id: surface.pane_id,
            anchor_pane_id: surface.anchor_pane_id,
            context_id: surface.context_id,
            client_id: surface.client_id.map(|client_id| client_id.0),
            x: surface.rect.x,
            y: surface.rect.y,
            w: surface.rect.w,
            h: surface.rect.h,
            scope: floating_scope_name(surface.scope).to_string(),
            layer: floating_layer_name(surface.layer).to_string(),
            z: surface.z,
            visible: surface.visible,
            opaque: surface.opaque,
            accepts_input: surface.accepts_input,
            cursor_owner: surface.cursor_owner,
        })
        .collect();
    Ok(FloatingPaneList {
        session_id: req.session_id,
        panes,
    })
}

pub fn get_pane(
    req: &GetPaneArgs,
    ctx: &NativeServiceContext,
) -> Result<PaneSummary, PaneStateError> {
    let list = list_panes(
        &ListPanesArgs {
            session_id: Some(req.session_id),
        },
        ctx,
    )?;
    list.panes
        .into_iter()
        .find(|p| p.id == req.pane_id)
        .ok_or(PaneStateError::PaneNotFound)
}

pub fn list_pane_processes() -> Result<PaneProcessList, PaneStateError> {
    let handle = super::session_runtime_handle().ok_or(PaneStateError::SessionNotFound)?;
    Ok(PaneProcessList {
        panes: handle
            .0
            .list_pane_processes()
            .into_iter()
            .map(|identity| to_api_process_identity(&identity))
            .collect(),
    })
}

pub fn get_pane_process(req: &GetPaneProcessArgs) -> Result<PaneProcessIdentity, PaneStateError> {
    let handle = super::session_runtime_handle().ok_or(PaneStateError::SessionNotFound)?;
    handle
        .0
        .pane_process_identity(SessionId(req.session_id), req.pane_id)
        .map(|identity| to_api_process_identity(&identity))
        .ok_or(PaneStateError::PaneNotFound)
}

const fn to_api_process_identity(
    value: &bmux_pane_runtime_state::PaneProcessIdentity,
) -> PaneProcessIdentity {
    PaneProcessIdentity {
        session_id: value.session_id.0,
        pane_id: value.pane_id,
        pid: value.pid,
        process_group_id: value.process_group_id,
    }
}
