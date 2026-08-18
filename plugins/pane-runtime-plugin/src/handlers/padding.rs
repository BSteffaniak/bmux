//! Pane-padding typed service handlers.

use bmux_pane_runtime_plugin_api::{
    pane_runtime_commands::PaneCommandError,
    pane_runtime_state::{PanePaddingSpec, PanePaddingState, PanePaddingStateError},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::padding_api::spec_from_api;
use crate::runtime::PanePaddingRuntimeHandle;
use bmux_session_models::{ClientId, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginPanePaddingPreviewArgs {
    pub pane_ids: Vec<Uuid>,
    pub padding: PanePaddingSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePanePaddingPreviewArgs {
    pub token: Uuid,
    pub pane_ids: Vec<Uuid>,
    pub padding: PanePaddingSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewTokenArgs {
    pub token: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitPanePaddingPreviewArgs {
    pub token: Uuid,
    pub persistence: bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPersistence,
}

fn owner_client_id(
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<ClientId, PaneCommandError> {
    context
        .caller_client_id
        .map(ClientId)
        .ok_or_else(|| PaneCommandError::Failed {
            reason: "pane padding preview requires an invoking client".to_string(),
        })
}

fn preview_state(
    token: Uuid,
    padding: &PanePaddingSpec,
    panes: Vec<crate::padding_api::RuntimePanePaddingState>,
) -> bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPreviewState {
    bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPreviewState {
        token,
        pane_ids: panes.iter().map(|pane| pane.pane_id).collect(),
        padding: padding.clone(),
        panes: panes
            .into_iter()
            .map(crate::padding_api::RuntimePanePaddingState::into_api)
            .collect(),
    }
}

pub fn begin_preview(
    req: &BeginPanePaddingPreviewArgs,
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPreviewState,
    PaneCommandError,
> {
    let spec = spec_from_api(&req.padding).map_err(|reason| PaneCommandError::Failed { reason })?;
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    let owner = owner_client_id(context)?;
    handle
        .begin_preview(owner, req.pane_ids.clone(), spec)
        .map(|(token, panes)| preview_state(token, &req.padding, panes))
        .map_err(|error| PaneCommandError::Failed {
            reason: error.to_string(),
        })
}

pub fn update_preview(
    req: &UpdatePanePaddingPreviewArgs,
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPreviewState,
    PaneCommandError,
> {
    let spec = spec_from_api(&req.padding).map_err(|reason| PaneCommandError::Failed { reason })?;
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    let owner = owner_client_id(context)?;
    handle
        .update_preview(owner, req.token, req.pane_ids.clone(), spec)
        .map(|panes| preview_state(req.token, &req.padding, panes))
        .map_err(|error| PaneCommandError::Failed {
            reason: error.to_string(),
        })
}

pub fn cancel_preview(
    req: &PreviewTokenArgs,
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPreviewToken,
    PaneCommandError,
> {
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    handle
        .cancel_preview(owner_client_id(context)?, req.token)
        .map(
            |()| bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPreviewToken {
                token: req.token,
            },
        )
        .map_err(|error| PaneCommandError::Failed {
            reason: error.to_string(),
        })
}

pub fn commit_preview(
    req: &CommitPanePaddingPreviewArgs,
    context: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<Vec<PanePaddingState>, PaneCommandError> {
    let persistence = match req.persistence {
        bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPersistence::Runtime => {
            crate::runtime::PanePaddingOverridePersistence::Runtime
        }
        bmux_pane_runtime_plugin_api::pane_runtime_commands::PanePaddingPersistence::Snapshot => {
            crate::runtime::PanePaddingOverridePersistence::Snapshot
        }
    };
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    handle
        .commit_preview(owner_client_id(context)?, req.token, persistence)
        .map(|panes| {
            panes
                .into_iter()
                .map(crate::padding_api::RuntimePanePaddingState::into_api)
                .collect()
        })
        .map_err(|error| PaneCommandError::Failed {
            reason: error.to_string(),
        })
}

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

fn map_command_error(handle: &PanePaddingRuntimeHandle, session_id: SessionId) -> PaneCommandError {
    if handle.has_session(session_id) {
        PaneCommandError::PaneNotFound
    } else {
        PaneCommandError::SessionNotFound
    }
}

fn map_state_error(
    handle: &PanePaddingRuntimeHandle,
    session_id: SessionId,
) -> PanePaddingStateError {
    if handle.has_session(session_id) {
        PanePaddingStateError::PaneNotFound
    } else {
        PanePaddingStateError::SessionNotFound
    }
}

pub fn get(req: &PanePaddingArgs) -> Result<PanePaddingState, PanePaddingStateError> {
    let handle = handle().ok_or(PanePaddingStateError::SessionNotFound)?;
    let session_id = SessionId(req.session_id);
    handle
        .state(session_id, req.pane_id)
        .map(crate::padding_api::RuntimePanePaddingState::into_api)
        .map_err(|_| map_state_error(&handle, session_id))
}

pub fn set(req: &SetPanePaddingArgs) -> Result<PanePaddingState, PaneCommandError> {
    let spec = spec_from_api(&req.padding).map_err(|reason| PaneCommandError::Failed { reason })?;
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    let session_id = SessionId(req.session_id);
    handle
        .set_override(session_id, req.pane_id, Some(spec))
        .map(crate::padding_api::RuntimePanePaddingState::into_api)
        .map_err(|_| map_command_error(&handle, session_id))
}

pub fn clear(req: &PanePaddingArgs) -> Result<PanePaddingState, PaneCommandError> {
    let handle = handle().ok_or(PaneCommandError::SessionNotFound)?;
    let session_id = SessionId(req.session_id);
    handle
        .set_override(session_id, req.pane_id, None)
        .map(crate::padding_api::RuntimePanePaddingState::into_api)
        .map_err(|_| map_command_error(&handle, session_id))
}
