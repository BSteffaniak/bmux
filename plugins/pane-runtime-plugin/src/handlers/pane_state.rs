//! Typed handlers for the `pane-runtime-state` interface.

use bmux_pane_runtime_plugin_api::pane_runtime_state::{
    PaneStateError, PaneSummary, SessionPaneList,
};
use bmux_session_models::SessionId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPanesArgs {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPaneArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
}

pub fn list_panes(req: &ListPanesArgs) -> Result<SessionPaneList, PaneStateError> {
    let handle = super::session_runtime_handle().ok_or(PaneStateError::SessionNotFound)?;
    let session_id = SessionId(req.session_id);
    let summaries = handle
        .0
        .list_panes(session_id)
        .map_err(|_| PaneStateError::SessionNotFound)?;
    Ok(SessionPaneList {
        session_id: req.session_id,
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

pub fn get_pane(req: &GetPaneArgs) -> Result<PaneSummary, PaneStateError> {
    let list = list_panes(&ListPanesArgs {
        session_id: req.session_id,
    })?;
    list.panes
        .into_iter()
        .find(|p| p.id == req.pane_id)
        .ok_or(PaneStateError::PaneNotFound)
}
