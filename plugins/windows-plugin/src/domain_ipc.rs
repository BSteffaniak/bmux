//! Private IPC helpers for windows-plugin.
//!
//! Provides the domain-shaped request/response types and a convenience
//! extension trait that wraps the generic
//! [`bmux_plugin::ServiceCaller`] with plugin-local ergonomic methods.
//! Windows is a foundational plugin (it owns pane/window state
//! alongside core's pane runtime) so it is permitted to reach core IPC
//! directly; this module encapsulates the encoding/decoding so the
//! rest of the plugin works in typed records.

#![allow(dead_code)]
#![allow(clippy::result_large_err)]

use bmux_clients_plugin_api::clients_state as api_clients_state;
use bmux_contexts_plugin_api::{contexts_commands, contexts_state as api_contexts_state};
use bmux_pane_runtime_plugin_api::{
    attach_runtime_commands as api_attach_runtime_commands,
    pane_runtime_commands as api_pane_runtime_commands,
    pane_runtime_state as api_pane_runtime_state,
};
use bmux_plugin::ServiceCaller;
use bmux_plugin_sdk::{PluginError, Result};
use bmux_sessions_plugin_api::sessions_state as api_sessions_state;
use bmux_windows_plugin_api::windows_events::{self, PaneEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Publish a `PaneEvent` on the typed plugin event bus. Silently
/// no-ops when the channel has not been registered (plugin not yet
/// activated, or the windows plugin is running in a context where
/// the bus is unavailable). Every windows-owned state transition
/// flows through one of these emits so subscriber plugins (notably
/// `bmux.decoration`) can reflect focus/zoom/open/close without a
/// follow-up query.
pub fn emit_pane_event(event: PaneEvent) {
    let _ = bmux_plugin::global_event_bus().emit(&windows_events::EVENT_KIND, event);
}

// ── Domain summary types ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub name: Option<String>,
    pub client_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSummary {
    pub id: Uuid,
    pub name: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: Uuid,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
}

// ── Selectors / directions ──────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSelector {
    ById(Uuid),
    ByName(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextSelector {
    ById(Uuid),
    ByName(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSelector {
    ById(Uuid),
    ByIndex(u32),
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirection {
    Next,
    Prev,
}

// ── Requests / responses ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreateRequest {
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreateResponse {
    pub id: Uuid,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSelectRequest {
    pub selector: SessionSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSelectResponse {
    pub session_id: Uuid,
    pub attach_token: Uuid,
    pub expires_at_epoch_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentClientResponse {
    pub id: Uuid,
    pub selected_session_id: Option<Uuid>,
    pub following_client_id: Option<Uuid>,
    pub following_global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCreateRequest {
    pub name: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCreateResponse {
    pub context: ContextSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextListResponse {
    pub contexts: Vec<ContextSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSelectRequest {
    pub selector: ContextSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSelectResponse {
    pub context: ContextSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCloseRequest {
    pub selector: ContextSelector,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCloseResponse {
    pub id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCurrentResponse {
    pub context: Option<ContextSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneListRequest {
    pub session: Option<SessionSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneListResponse {
    pub panes: Vec<PaneSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSplitRequest {
    pub session: Option<SessionSelector>,
    pub target: Option<PaneSelector>,
    pub direction: PaneSplitDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLaunchCommand {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLaunchRequest {
    pub session: Option<SessionSelector>,
    pub target: Option<PaneSelector>,
    pub direction: PaneSplitDirection,
    pub name: Option<String>,
    pub command: PaneLaunchCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSplitResponse {
    pub id: Uuid,
    pub session_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLaunchResponse {
    pub id: Uuid,
    pub session_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneFocusRequest {
    pub session: Option<SessionSelector>,
    pub target: Option<PaneSelector>,
    pub direction: Option<PaneFocusDirection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneFocusResponse {
    pub id: Uuid,
    pub session_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneResizeRequest {
    pub session: Option<SessionSelector>,
    pub target: Option<PaneSelector>,
    pub direction: PaneResizeDirection,
    pub cells: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneResizeDirection {
    Increase,
    Decrease,
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneResizeResponse {
    pub session_id: Uuid,
}

const fn pane_resize_direction_name(direction: PaneResizeDirection) -> &'static str {
    match direction {
        PaneResizeDirection::Increase => "increase",
        PaneResizeDirection::Decrease => "decrease",
        PaneResizeDirection::Left => "left",
        PaneResizeDirection::Right => "right",
        PaneResizeDirection::Up => "up",
        PaneResizeDirection::Down => "down",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCloseRequest {
    pub session: Option<SessionSelector>,
    pub target: Option<PaneSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCloseResponse {
    pub id: Uuid,
    pub session_id: Uuid,
    pub session_closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneZoomRequest {
    #[serde(default)]
    pub session: Option<SessionSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneZoomResponse {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub zoomed: bool,
}

// ── SDK → IPC selector converters ───────────────────────────────────

fn session_selector_to_ipc(selector: &SessionSelector) -> bmux_ipc::SessionSelector {
    match selector {
        SessionSelector::ById(id) => bmux_ipc::SessionSelector::ById(*id),
        SessionSelector::ByName(name) => bmux_ipc::SessionSelector::ByName(name.clone()),
    }
}

#[allow(dead_code)]
fn context_selector_to_ipc(selector: &ContextSelector) -> bmux_ipc::ContextSelector {
    match selector {
        ContextSelector::ById(id) => bmux_ipc::ContextSelector::ById(*id),
        ContextSelector::ByName(name) => bmux_ipc::ContextSelector::ByName(name.clone()),
    }
}

const fn pane_selector_to_ipc(selector: &PaneSelector) -> bmux_ipc::PaneSelector {
    match selector {
        PaneSelector::ById(id) => bmux_ipc::PaneSelector::ById(*id),
        PaneSelector::ByIndex(index) => bmux_ipc::PaneSelector::ByIndex(*index),
        PaneSelector::Active => bmux_ipc::PaneSelector::Active,
    }
}

const fn split_direction_to_ipc(direction: PaneSplitDirection) -> bmux_ipc::PaneSplitDirection {
    match direction {
        PaneSplitDirection::Vertical => bmux_ipc::PaneSplitDirection::Vertical,
        PaneSplitDirection::Horizontal => bmux_ipc::PaneSplitDirection::Horizontal,
    }
}

const fn focus_direction_to_ipc(direction: PaneFocusDirection) -> bmux_ipc::PaneFocusDirection {
    match direction {
        PaneFocusDirection::Next => bmux_ipc::PaneFocusDirection::Next,
        PaneFocusDirection::Prev => bmux_ipc::PaneFocusDirection::Prev,
    }
}

fn unexpected(operation: &'static str) -> PluginError {
    PluginError::ServiceProtocol {
        details: format!("unexpected response payload for {operation}"),
    }
}

fn api_context_summary_to_local(summary: api_contexts_state::ContextSummary) -> ContextSummary {
    ContextSummary {
        id: summary.id,
        name: summary.name,
        attributes: summary.attributes,
    }
}

fn api_session_summary_to_local(summary: api_sessions_state::SessionSummary) -> SessionSummary {
    SessionSummary {
        id: summary.id,
        name: summary.name,
        client_count: summary.client_count as usize,
    }
}

fn session_selector_to_api(selector: &SessionSelector) -> api_sessions_state::SessionSelector {
    match selector {
        SessionSelector::ById(id) => api_sessions_state::SessionSelector {
            id: Some(*id),
            name: None,
        },
        SessionSelector::ByName(name) => api_sessions_state::SessionSelector {
            id: None,
            name: Some(name.clone()),
        },
    }
}

fn session_selector_to_attach_api(
    selector: &SessionSelector,
) -> api_attach_runtime_commands::SessionSelector {
    match selector {
        SessionSelector::ById(id) => api_attach_runtime_commands::SessionSelector {
            id: Some(*id),
            name: None,
        },
        SessionSelector::ByName(name) => api_attach_runtime_commands::SessionSelector {
            id: None,
            name: Some(name.clone()),
        },
    }
}

fn context_selector_to_api(selector: &ContextSelector) -> api_contexts_state::ContextSelector {
    match selector {
        ContextSelector::ById(id) => api_contexts_state::ContextSelector {
            id: Some(*id),
            name: None,
        },
        ContextSelector::ByName(name) => api_contexts_state::ContextSelector {
            id: None,
            name: Some(name.clone()),
        },
    }
}

fn typed_context_error(operation: &'static str, err: impl std::fmt::Display) -> PluginError {
    PluginError::ServiceProtocol {
        details: format!("{operation} failed: {err}"),
    }
}

fn typed_service_error(operation: &'static str, err: impl std::fmt::Display) -> PluginError {
    PluginError::ServiceProtocol {
        details: format!("{operation} failed: {err}"),
    }
}

fn pane_target_uuid(selector: Option<&PaneSelector>) -> Option<Uuid> {
    selector.and_then(|sel| match sel {
        PaneSelector::ById(id) => Some(*id),
        PaneSelector::ByIndex(_) | PaneSelector::Active => None,
    })
}

// ── Extension trait ─────────────────────────────────────────────────

/// Extension trait bundling core-IPC helpers for session/pane/context/
/// client operations. Each method wraps a call to
/// [`ServiceCaller::execute_kernel_request`] with a typed request/
/// response shape.
///
/// Blanket-implemented for all `T: ServiceCaller + ?Sized`; this
/// plugin brings it into scope with `use crate::domain_ipc::KernelOps;`.
pub trait KernelOps: ServiceCaller {
    /// List all sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_list(&self) -> Result<SessionListResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let entries = bmux_plugin::block_on_typed_dispatch(
            api_sessions_state::client::list_sessions(&mut client),
        )
        .map_err(|err| typed_service_error("session_list", err))?;
        Ok(SessionListResponse {
            sessions: entries
                .into_iter()
                .map(api_session_summary_to_local)
                .collect(),
        })
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_create(&self, request: &SessionCreateRequest) -> Result<SessionCreateResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result = bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_commands::client::new_session_with_runtime(
                &mut client,
                request.name.clone(),
            ),
        )
        .map_err(|err| typed_service_error("new-session-with-runtime", err))?;
        match result {
            Ok(ack) => Ok(SessionCreateResponse {
                id: ack.session_id,
                name: request.name.clone(),
            }),
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("new-session-with-runtime failed: {err:?}"),
            }),
        }
    }

    /// Select (attach to) a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_select(&self, request: &SessionSelectRequest) -> Result<SessionSelectResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result = bmux_plugin::block_on_typed_dispatch(
            api_attach_runtime_commands::client::attach_session(
                &mut client,
                session_selector_to_attach_api(&request.selector),
                true,
            ),
        )
        .map_err(|err| typed_service_error("attach-session", err))?;
        match result {
            Ok(grant) => Ok(SessionSelectResponse {
                session_id: grant.session_id,
                attach_token: grant.token,
                expires_at_epoch_ms: grant.expires_epoch_ms,
            }),
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("attach-session failed: {err:?}"),
            }),
        }
    }

    /// Get the current client identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn current_client(&self) -> Result<CurrentClientResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        bmux_plugin::block_on_typed_dispatch(api_clients_state::client::current_client(&mut client))
            .map_err(|err| typed_service_error("current_client", err))?
            .map_or_else(
                |_| Err(unexpected("current_client")),
                |summary| {
                    Ok(CurrentClientResponse {
                        id: summary.id,
                        selected_session_id: summary.selected_session_id,
                        following_client_id: summary.following_client_id,
                        following_global: summary.following_global,
                    })
                },
            )
    }

    /// List all contexts.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_list(&self) -> Result<ContextListResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let contexts = bmux_plugin::block_on_typed_dispatch(
            api_contexts_state::client::list_contexts(&mut client),
        )
        .map_err(|err| typed_context_error("context_list", err))?
        .into_iter()
        .map(api_context_summary_to_local)
        .collect();
        Ok(ContextListResponse { contexts })
    }

    /// Get the current context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_current(&self) -> Result<ContextCurrentResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let context = bmux_plugin::block_on_typed_dispatch(
            api_contexts_state::client::current_context(&mut client),
        )
        .map_err(|err| typed_context_error("context_current", err))?
        .map(api_context_summary_to_local);
        Ok(ContextCurrentResponse { context })
    }

    /// Create a new context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_create(&self, request: &ContextCreateRequest) -> Result<ContextCreateResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result =
            bmux_plugin::block_on_typed_dispatch(contexts_commands::client::create_context(
                &mut client,
                request.name.clone(),
                request.attributes.clone(),
            ))
            .map_err(|err| typed_context_error("context_create", err))?;
        match result {
            Ok(ack) => Ok(ContextCreateResponse {
                context: ContextSummary {
                    id: ack.id,
                    name: request.name.clone(),
                    attributes: request.attributes.clone(),
                },
            }),
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("context_create failed: {err:?}"),
            }),
        }
    }

    /// Select (switch to) a context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_select(&self, request: &ContextSelectRequest) -> Result<ContextSelectResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result =
            bmux_plugin::block_on_typed_dispatch(contexts_commands::client::select_context(
                &mut client,
                context_selector_to_api(&request.selector),
            ))
            .map_err(|err| typed_context_error("context_select", err))?;
        let ack = result.map_err(|err| PluginError::ServiceProtocol {
            details: format!("context_select failed: {err:?}"),
        })?;
        Ok(ContextSelectResponse {
            context: ContextSummary {
                id: ack.id,
                name: None,
                attributes: std::collections::BTreeMap::new(),
            },
        })
    }

    /// Close a context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_close(&self, request: &ContextCloseRequest) -> Result<ContextCloseResponse>
    where
        Self: Sync,
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result =
            bmux_plugin::block_on_typed_dispatch(contexts_commands::client::close_context(
                &mut client,
                context_selector_to_api(&request.selector),
                request.force,
            ))
            .map_err(|err| typed_context_error("context_close", err))?;
        let ack = result.map_err(|err| PluginError::ServiceProtocol {
            details: format!("context_close failed: {err:?}"),
        })?;
        Ok(ContextCloseResponse { id: ack.id })
    }

    /// List panes.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_list(&self, request: &PaneListRequest) -> Result<PaneListResponse>
    where
        Self: Sync,
    {
        let Some(SessionSelector::ById(session_id)) = request.session.clone() else {
            return Err(PluginError::ServiceProtocol {
                details: "pane_list requires a by-id session selector in typed dispatch"
                    .to_string(),
            });
        };
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result = bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_state::client::list_panes(&mut client, Some(session_id)),
        )
        .map_err(|err| typed_service_error("list-panes", err))?;
        match result {
            Ok(panes) => Ok(PaneListResponse {
                panes: panes
                    .panes
                    .into_iter()
                    .enumerate()
                    .map(|(idx, p)| PaneSummary {
                        id: p.id,
                        index: u32::try_from(idx).unwrap_or(0),
                        name: p.name,
                        focused: p.focused,
                    })
                    .collect(),
            }),
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("list-panes failed: {err:?}"),
            }),
        }
    }

    /// Resolve a session selector to a concrete `Uuid` by looking up
    /// by-name selectors against the sessions-state typed service.
    /// `None` selectors are treated as "use the caller's selected
    /// session" — the caller must handle resolution of that state;
    /// here we return an error so the call site surfaces a clear
    /// protocol violation.
    ///
    /// # Errors
    ///
    /// Returns an error when the typed service call fails or a
    /// name-based selector doesn't match a known session.
    fn resolve_session_uuid(&self, selector: Option<&SessionSelector>) -> Result<Uuid>
    where
        Self: Sync,
    {
        match selector {
            Some(SessionSelector::ById(id)) => Ok(*id),
            Some(SessionSelector::ByName(name)) => {
                let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
                let entries = bmux_plugin::block_on_typed_dispatch(
                    api_sessions_state::client::list_sessions(&mut client),
                )
                .map_err(|err| typed_service_error("resolve_session_uuid", err))?;
                entries
                    .into_iter()
                    .find(|e| e.name.as_deref() == Some(name.as_str()))
                    .map(|e| e.id)
                    .ok_or_else(|| PluginError::ServiceProtocol {
                        details: format!("session '{name}' not found"),
                    })
            }
            None => Err(PluginError::ServiceProtocol {
                details: "pane operations require an explicit session selector (typed dispatch \
                          does not carry the caller's selected-session state)"
                    .to_string(),
            }),
        }
    }

    /// Split a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_split(&self, request: &PaneSplitRequest) -> Result<PaneSplitResponse>
    where
        Self: Sync,
    {
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = pane_target_uuid(request.target.as_ref());
        let direction = match request.direction {
            PaneSplitDirection::Horizontal => "horizontal",
            PaneSplitDirection::Vertical => "vertical",
        };
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result =
            bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::split_pane(
                &mut client,
                session_id,
                target,
                direction.to_string(),
                50,
            ))
            .map_err(|err| typed_service_error("split-pane", err))?;
        match result {
            Ok(ack) => {
                emit_pane_event(PaneEvent::Opened {
                    pane_id: ack.pane_id,
                    session_id: ack.session_id,
                });
                Ok(PaneSplitResponse {
                    id: ack.pane_id,
                    session_id: ack.session_id,
                })
            }
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("split-pane failed: {err:?}"),
            }),
        }
    }

    /// Launch a pane with explicit command metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_launch(&self, request: &PaneLaunchRequest) -> Result<PaneLaunchResponse>
    where
        Self: Sync,
    {
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = pane_target_uuid(request.target.as_ref());
        let direction = match request.direction {
            PaneSplitDirection::Horizontal => "horizontal",
            PaneSplitDirection::Vertical => "vertical",
        };
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result =
            bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::launch_pane(
                &mut client,
                session_id,
                target,
                direction.to_string(),
                50,
                request.name.clone(),
                request.command.program.clone(),
                request.command.args.clone(),
                request.command.cwd.clone(),
            ))
            .map_err(|err| typed_service_error("launch-pane", err))?;
        match result {
            Ok(ack) => {
                emit_pane_event(PaneEvent::Opened {
                    pane_id: ack.pane_id,
                    session_id: ack.session_id,
                });
                Ok(PaneLaunchResponse {
                    id: ack.pane_id,
                    session_id: ack.session_id,
                })
            }
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("launch-pane failed: {err:?}"),
            }),
        }
    }

    /// Focus a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_focus(&self, request: &PaneFocusRequest) -> Result<PaneFocusResponse>
    where
        Self: Sync,
    {
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = pane_target_uuid(request.target.as_ref());
        let direction = request.direction.map_or_else(String::new, |d| match d {
            PaneFocusDirection::Next => "next".to_string(),
            PaneFocusDirection::Prev => "prev".to_string(),
        });
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result =
            bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::focus_pane(
                &mut client,
                session_id,
                target,
                direction,
            ))
            .map_err(|err| typed_service_error("focus-pane", err))?;
        match result {
            Ok(ack) => {
                emit_pane_event(PaneEvent::Focused {
                    pane_id: ack.pane_id,
                });
                Ok(PaneFocusResponse {
                    id: ack.pane_id,
                    session_id: ack.session_id,
                })
            }
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("focus-pane failed: {err:?}"),
            }),
        }
    }

    /// Resize a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_resize(&self, request: &PaneResizeRequest) -> Result<PaneResizeResponse>
    where
        Self: Sync,
    {
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = pane_target_uuid(request.target.as_ref());
        let direction = pane_resize_direction_name(request.direction);
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result =
            bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::resize_pane(
                &mut client,
                session_id,
                target,
                direction.to_string(),
                request.cells,
            ))
            .map_err(|err| typed_service_error("resize-pane", err))?;
        match result {
            Ok(ack) => Ok(PaneResizeResponse {
                session_id: ack.session_id,
            }),
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("resize-pane failed: {err:?}"),
            }),
        }
    }

    /// Close a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse>
    where
        Self: Sync,
    {
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = pane_target_uuid(request.target.as_ref());
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result = bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_commands::client::close_pane(&mut client, session_id, target),
        )
        .map_err(|err| typed_service_error("close-pane", err))?;
        match result {
            Ok(ack) => {
                emit_pane_event(PaneEvent::Closed {
                    pane_id: ack.pane_id,
                });
                Ok(PaneCloseResponse {
                    id: ack.pane_id,
                    session_id: ack.session_id,
                    // Pane-runtime close-pane doesn't report whether the
                    // session itself was removed; the caller (windows
                    // plugin) no longer depends on this flag because
                    // session teardown is orchestrated inside the pane
                    // runtime plugin.
                    session_closed: false,
                })
            }
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("close-pane failed: {err:?}"),
            }),
        }
    }

    /// Toggle the zoom state of the currently-active pane in the
    /// targeted session (or the selected session when none is given).
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_zoom(&self, request: &PaneZoomRequest) -> Result<PaneZoomResponse>
    where
        Self: Sync,
    {
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(self);
        let result = bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_commands::client::zoom_pane(&mut client, session_id),
        )
        .map_err(|err| typed_service_error("zoom-pane", err))?;
        match result {
            Ok(ack) => {
                // `zoomed` in the ack is informational; the pane-runtime
                // flips the flag atomically. We emit the currently-claimed
                // direction; if the runtime semantics change later, we'd
                // thread the previous state through via a query first.
                emit_pane_event(PaneEvent::Zoomed {
                    pane_id: ack.pane_id,
                });
                Ok(PaneZoomResponse {
                    session_id: ack.session_id,
                    pane_id: ack.pane_id,
                    zoomed: true,
                })
            }
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("zoom-pane failed: {err:?}"),
            }),
        }
    }
}

impl<T: ServiceCaller + ?Sized> KernelOps for T {}
