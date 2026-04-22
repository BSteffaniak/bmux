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

use bmux_plugin::ServiceCaller;
use bmux_plugin_sdk::{PluginError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

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
    pub delta: i16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneResizeResponse {
    pub session_id: Uuid,
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
    fn session_list(&self) -> Result<SessionListResponse> {
        // Dispatch through sessions-plugin's typed
        // `sessions-state::list-sessions` service rather than the
        // legacy `Request::ListSessions` IPC variant.
        #[derive(Deserialize)]
        struct Entry {
            id: Uuid,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            client_count: u32,
        }
        let entries: Vec<Entry> = self.call_service(
            bmux_sessions_plugin_api::capabilities::SESSIONS_READ.as_str(),
            bmux_plugin_sdk::ServiceKind::Query,
            bmux_sessions_plugin_api::sessions_state::INTERFACE_ID.as_str(),
            "list-sessions",
            &(),
        )?;
        Ok(SessionListResponse {
            sessions: entries
                .into_iter()
                .map(|e| SessionSummary {
                    id: e.id,
                    name: e.name,
                    client_count: e.client_count as usize,
                })
                .collect(),
        })
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_create(&self, request: &SessionCreateRequest) -> Result<SessionCreateResponse> {
        #[derive(Serialize)]
        struct Args {
            name: Option<String>,
        }
        let result: std::result::Result<
            bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionAck,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionRuntimeCommandError,
        > = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
            "new-session-with-runtime",
            &Args {
                name: request.name.clone(),
            },
        )?;
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
    fn session_select(&self, request: &SessionSelectRequest) -> Result<SessionSelectResponse> {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Args {
            selector: bmux_ipc::SessionSelector,
            can_write: bool,
        }
        let result = self.call_service::<Args, std::result::Result<
            bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachGrant,
            bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachCommandError,
        >>(
            bmux_pane_runtime_plugin_api::capabilities::ATTACH_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::attach_runtime_commands::INTERFACE_ID.as_str(),
            "attach-session",
            &Args {
                selector: session_selector_to_ipc(&request.selector),
                can_write: true,
            },
        )?;
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
    fn current_client(&self) -> Result<CurrentClientResponse> {
        use bmux_clients_plugin_api::clients_state::{self, ClientQueryError, ClientSummary};
        self.call_service::<(), std::result::Result<ClientSummary, ClientQueryError>>(
            bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str(),
            bmux_plugin_sdk::ServiceKind::Query,
            clients_state::INTERFACE_ID.as_str(),
            "current-client",
            &(),
        )?
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
    fn context_list(&self) -> Result<ContextListResponse> {
        // Route through the contexts-plugin typed surface — the
        // `Request::ListContexts` IPC variant was removed.
        let contexts: Vec<ContextSummary> = self.call_service(
            "bmux.contexts.read",
            bmux_plugin_sdk::ServiceKind::Query,
            "contexts-state",
            "list-contexts",
            &(),
        )?;
        Ok(ContextListResponse { contexts })
    }

    /// Get the current context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_current(&self) -> Result<ContextCurrentResponse> {
        let context: Option<ContextSummary> = self.call_service(
            "bmux.contexts.read",
            bmux_plugin_sdk::ServiceKind::Query,
            "contexts-state",
            "current-context",
            &(),
        )?;
        Ok(ContextCurrentResponse { context })
    }

    /// Create a new context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_create(&self, request: &ContextCreateRequest) -> Result<ContextCreateResponse> {
        #[derive(serde::Serialize)]
        struct Args<'a> {
            name: &'a Option<String>,
            attributes: &'a std::collections::BTreeMap<String, String>,
        }
        #[derive(serde::Deserialize)]
        struct Ack {
            id: ::uuid::Uuid,
        }
        #[derive(serde::Deserialize, Debug)]
        #[serde(rename_all = "snake_case")]
        enum CreateErr {
            NameAlreadyExists { name: String },
            InvalidName { reason: String },
            Failed { reason: String },
        }
        impl std::fmt::Display for CreateErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::NameAlreadyExists { name } => write!(f, "name already exists: {name}"),
                    Self::InvalidName { reason } => write!(f, "invalid name: {reason}"),
                    Self::Failed { reason } => write!(f, "{reason}"),
                }
            }
        }
        let result: std::result::Result<Ack, CreateErr> = self.call_service(
            "bmux.contexts.write",
            bmux_plugin_sdk::ServiceKind::Command,
            "contexts-commands",
            "create-context",
            &Args {
                name: &request.name,
                attributes: &request.attributes,
            },
        )?;
        match result {
            Ok(ack) => Ok(ContextCreateResponse {
                context: ContextSummary {
                    id: ack.id,
                    name: request.name.clone(),
                    attributes: request.attributes.clone(),
                },
            }),
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("context_create failed: {err}"),
            }),
        }
    }

    /// Select (switch to) a context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_select(&self, request: &ContextSelectRequest) -> Result<ContextSelectResponse> {
        #[derive(serde::Serialize)]
        struct Selector {
            id: Option<::uuid::Uuid>,
            name: Option<String>,
        }
        #[derive(serde::Serialize)]
        struct Args {
            selector: Selector,
        }
        #[derive(serde::Deserialize)]
        struct Ack {
            id: ::uuid::Uuid,
        }
        // Mirror of the BPDL-generated `SelectContextError` shape so
        // we can decode without pulling in the contexts-plugin-api
        // crate (which would be an odd dep direction).
        #[derive(serde::Deserialize, Debug)]
        #[serde(rename_all = "snake_case")]
        enum SelectErr {
            NotFound,
            Denied { reason: String },
        }
        impl std::fmt::Display for SelectErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::NotFound => write!(f, "not_found"),
                    Self::Denied { reason } => write!(f, "{reason}"),
                }
            }
        }
        let (id, name) = match &request.selector {
            ContextSelector::ById(id) => (Some(*id), None),
            ContextSelector::ByName(n) => (None, Some(n.clone())),
        };
        let result: std::result::Result<Ack, SelectErr> = self.call_service(
            "bmux.contexts.write",
            bmux_plugin_sdk::ServiceKind::Command,
            "contexts-commands",
            "select-context",
            &Args {
                selector: Selector { id, name },
            },
        )?;
        let ack = result.map_err(|err| PluginError::ServiceProtocol {
            details: format!("context_select failed: {err}"),
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
    fn context_close(&self, request: &ContextCloseRequest) -> Result<ContextCloseResponse> {
        #[derive(serde::Serialize)]
        struct Selector {
            id: Option<::uuid::Uuid>,
            name: Option<String>,
        }
        #[derive(serde::Serialize)]
        struct Args {
            selector: Selector,
            force: bool,
        }
        #[derive(serde::Deserialize)]
        struct Ack {
            id: ::uuid::Uuid,
        }
        #[derive(serde::Deserialize, Debug)]
        #[serde(rename_all = "snake_case")]
        enum CloseErr {
            NotFound,
            Denied { reason: String },
            Failed { reason: String },
        }
        impl std::fmt::Display for CloseErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::NotFound => write!(f, "not_found"),
                    Self::Denied { reason } | Self::Failed { reason } => write!(f, "{reason}"),
                }
            }
        }
        let (id, name) = match &request.selector {
            ContextSelector::ById(i) => (Some(*i), None),
            ContextSelector::ByName(n) => (None, Some(n.clone())),
        };
        let result: std::result::Result<Ack, CloseErr> = self.call_service(
            "bmux.contexts.write",
            bmux_plugin_sdk::ServiceKind::Command,
            "contexts-commands",
            "close-context",
            &Args {
                selector: Selector { id, name },
                force: request.force,
            },
        )?;
        let ack = result.map_err(|err| PluginError::ServiceProtocol {
            details: format!("context_close failed: {err}"),
        })?;
        Ok(ContextCloseResponse { id: ack.id })
    }

    /// List panes.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_list(&self, request: &PaneListRequest) -> Result<PaneListResponse> {
        // Dispatch through the pane-runtime plugin's typed
        // `pane-runtime-state::list-panes` service rather than the
        // legacy `Request::ListPanes` IPC variant.
        #[derive(Serialize)]
        struct Args {
            session_id: Uuid,
        }
        #[derive(Deserialize)]
        struct Panes {
            panes: Vec<PaneEntry>,
        }
        #[derive(Deserialize)]
        struct PaneEntry {
            id: Uuid,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            focused: bool,
        }

        let Some(SessionSelector::ById(session_id)) = request.session.clone() else {
            return Err(PluginError::ServiceProtocol {
                details: "pane_list requires a by-id session selector in typed dispatch"
                    .to_string(),
            });
        };
        let result: std::result::Result<Panes, serde_json::Value> = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_READ.as_str(),
            bmux_plugin_sdk::ServiceKind::Query,
            bmux_pane_runtime_plugin_api::pane_runtime_state::INTERFACE_ID.as_str(),
            "list-panes",
            &Args { session_id },
        )?;
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
                details: format!("list-panes typed dispatch returned error: {err}"),
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
    fn resolve_session_uuid(&self, selector: Option<&SessionSelector>) -> Result<Uuid> {
        match selector {
            Some(SessionSelector::ById(id)) => Ok(*id),
            Some(SessionSelector::ByName(name)) => {
                #[derive(Deserialize)]
                struct Entry {
                    id: Uuid,
                    #[serde(default)]
                    name: Option<String>,
                }
                let entries: Vec<Entry> = self.call_service(
                    bmux_sessions_plugin_api::capabilities::SESSIONS_READ.as_str(),
                    bmux_plugin_sdk::ServiceKind::Query,
                    bmux_sessions_plugin_api::sessions_state::INTERFACE_ID.as_str(),
                    "list-sessions",
                    &(),
                )?;
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
    fn pane_split(&self, request: &PaneSplitRequest) -> Result<PaneSplitResponse> {
        #[derive(Serialize)]
        struct Args {
            session_id: Uuid,
            target: Option<Uuid>,
            direction: String,
            ratio_percent: u8,
        }
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = request.target.as_ref().and_then(|sel| match sel {
            PaneSelector::ById(id) => Some(*id),
            _ => None,
        });
        let direction = match request.direction {
            PaneSplitDirection::Horizontal => "horizontal",
            PaneSplitDirection::Vertical => "vertical",
        };
        let result: std::result::Result<
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneAck,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneCommandError,
        > = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
            "split-pane",
            &Args {
                session_id,
                target,
                direction: direction.to_string(),
                ratio_percent: 50,
            },
        )?;
        match result {
            Ok(ack) => Ok(PaneSplitResponse {
                id: ack.pane_id,
                session_id: ack.session_id,
            }),
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
    fn pane_launch(&self, request: &PaneLaunchRequest) -> Result<PaneLaunchResponse> {
        #[derive(Serialize)]
        #[allow(
            clippy::struct_field_names,
            reason = "Args field names mirror the BPDL contract fields"
        )]
        struct Args {
            session_id: Uuid,
            target: Option<Uuid>,
            direction: String,
            ratio_percent: u8,
            name: Option<String>,
            program: String,
            args: Vec<String>,
            cwd: Option<String>,
        }
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = request.target.as_ref().and_then(|sel| match sel {
            PaneSelector::ById(id) => Some(*id),
            _ => None,
        });
        let direction = match request.direction {
            PaneSplitDirection::Horizontal => "horizontal",
            PaneSplitDirection::Vertical => "vertical",
        };
        let result: std::result::Result<
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneAck,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneCommandError,
        > = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
            "launch-pane",
            &Args {
                session_id,
                target,
                direction: direction.to_string(),
                ratio_percent: 50,
                name: request.name.clone(),
                program: request.command.program.clone(),
                args: request.command.args.clone(),
                cwd: request.command.cwd.clone(),
            },
        )?;
        match result {
            Ok(ack) => Ok(PaneLaunchResponse {
                id: ack.pane_id,
                session_id: ack.session_id,
            }),
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
    fn pane_focus(&self, request: &PaneFocusRequest) -> Result<PaneFocusResponse> {
        #[derive(Serialize)]
        struct Args {
            session_id: Uuid,
            target: Option<Uuid>,
            direction: String,
        }
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = request.target.as_ref().and_then(|sel| match sel {
            PaneSelector::ById(id) => Some(*id),
            _ => None,
        });
        let direction = request.direction.map_or_else(String::new, |d| match d {
            PaneFocusDirection::Next => "next".to_string(),
            PaneFocusDirection::Prev => "prev".to_string(),
        });
        let result: std::result::Result<
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneAck,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneCommandError,
        > = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
            "focus-pane",
            &Args {
                session_id,
                target,
                direction,
            },
        )?;
        match result {
            Ok(ack) => Ok(PaneFocusResponse {
                id: ack.pane_id,
                session_id: ack.session_id,
            }),
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
    fn pane_resize(&self, request: &PaneResizeRequest) -> Result<PaneResizeResponse> {
        #[derive(Serialize)]
        struct Args {
            session_id: Uuid,
            target: Option<Uuid>,
            delta_percent: i8,
        }
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = request.target.as_ref().and_then(|sel| match sel {
            PaneSelector::ById(id) => Some(*id),
            _ => None,
        });
        let delta_percent =
            i8::try_from(request.delta).unwrap_or(if request.delta < 0 { -50 } else { 50 });
        let result: std::result::Result<
            bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionAck,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneCommandError,
        > = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
            "resize-pane",
            &Args {
                session_id,
                target,
                delta_percent,
            },
        )?;
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
    fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse> {
        #[derive(Serialize)]
        struct Args {
            session_id: Uuid,
            target: Option<Uuid>,
        }
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let target = request.target.as_ref().and_then(|sel| match sel {
            PaneSelector::ById(id) => Some(*id),
            _ => None,
        });
        let result: std::result::Result<
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneAck,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneCommandError,
        > = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
            "close-pane",
            &Args { session_id, target },
        )?;
        match result {
            Ok(ack) => Ok(PaneCloseResponse {
                id: ack.pane_id,
                session_id: ack.session_id,
                // Pane-runtime close-pane doesn't report whether the
                // session itself was removed; the caller (windows
                // plugin) no longer depends on this flag because
                // session teardown is orchestrated inside the pane
                // runtime plugin.
                session_closed: false,
            }),
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
    fn pane_zoom(&self, request: &PaneZoomRequest) -> Result<PaneZoomResponse> {
        #[derive(Serialize)]
        struct Args {
            session_id: Uuid,
        }
        let session_id = self.resolve_session_uuid(request.session.as_ref())?;
        let result: std::result::Result<
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneAck,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneCommandError,
        > = self.call_service(
            bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
            bmux_plugin_sdk::ServiceKind::Command,
            bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
            "zoom-pane",
            &Args { session_id },
        )?;
        match result {
            Ok(ack) => Ok(PaneZoomResponse {
                session_id: ack.session_id,
                pane_id: ack.pane_id,
                zoomed: true,
            }),
            Err(err) => Err(PluginError::ServiceProtocol {
                details: format!("zoom-pane failed: {err:?}"),
            }),
        }
    }
}

impl<T: ServiceCaller + ?Sized> KernelOps for T {}
