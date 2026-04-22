//! Typed handlers for the `pane-runtime-state` interface.

use bmux_client_state::FollowStateHandle;
use bmux_pane_runtime_plugin_api::pane_runtime_state::{
    PaneStateError, PaneSummary, SessionPaneList,
};
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
