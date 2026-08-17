//! Pane-padding typed service handlers.

use bmux_pane_runtime_plugin_api::{
    pane_runtime_commands::PaneCommandError,
    pane_runtime_state::{PanePaddingSpec, PanePaddingState, PanePaddingStateError},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::padding_api::spec_from_api;
use crate::runtime::PanePaddingRuntimeHandle;
use bmux_session_models::SessionId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanePaddingArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub pane_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPanePaddingArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub pane_id: Option<Uuid>,
    pub padding: PanePaddingSpec,
}

fn handle() -> Option<PanePaddingRuntimeHandle> {
    bmux_plugin::global_plugin_state_registry()
        .get::<PanePaddingRuntimeHandle>()
        .and_then(|entry| entry.read().ok().map(|guard| (*guard).clone()))
}

pub fn get(req: &PanePaddingArgs) -> Result<PanePaddingState, PanePaddingStateError> {
    let handle = handle().ok_or(PanePaddingStateError::SessionNotFound)?;
    handle
        .state(SessionId(req.session_id), req.pane_id)
        .map(crate::padding_api::RuntimePanePaddingState::into_api)
        .map_err(|_| PanePaddingStateError::PaneNotFound)
}

pub fn set(req: &SetPanePaddingArgs) -> Result<PanePaddingState, PaneCommandError> {
    let spec = spec_from_api(&req.padding).map_err(|reason| PaneCommandError::Failed { reason })?;
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    handle
        .set_override(SessionId(req.session_id), req.pane_id, Some(spec))
        .map(crate::padding_api::RuntimePanePaddingState::into_api)
        .map_err(|_| PaneCommandError::PaneNotFound)
}

pub fn clear(req: &PanePaddingArgs) -> Result<PanePaddingState, PaneCommandError> {
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    handle
        .set_override(SessionId(req.session_id), req.pane_id, None)
        .map(crate::padding_api::RuntimePanePaddingState::into_api)
        .map_err(|_| PaneCommandError::PaneNotFound)
}
