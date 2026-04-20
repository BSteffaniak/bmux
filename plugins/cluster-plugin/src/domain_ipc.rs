//! Private IPC helpers for cluster-plugin.
//!
//! cluster-plugin is a non-foundational plugin; in a stricter
//! architecture it would consume its session/pane data via typed
//! plugin-api dispatch. For now, this module encapsulates the
//! direct-IPC bridge that routes through the sessions / windows /
//! contexts plugins. Once every operation has a typed equivalent the
//! module shrinks toward zero.

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

/// Opt-in extension trait providing domain-shaped convenience methods
/// on top of [`ServiceCaller::execute_kernel_request`].
///
/// Blanket-implemented for all `T: ServiceCaller + ?Sized`; plugins
/// bring it into scope with `use crate::domain_ipc::DomainCompat;`.
pub trait DomainCompat: ServiceCaller {
    /// List all sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_list(&self) -> Result<SessionListResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::ListSessions)? {
            bmux_ipc::ResponsePayload::SessionList { sessions } => Ok(SessionListResponse {
                sessions: sessions
                    .into_iter()
                    .map(|s| SessionSummary {
                        id: s.id,
                        name: s.name,
                        client_count: s.client_count,
                    })
                    .collect(),
            }),
            _ => Err(unexpected("session_list")),
        }
    }

    /// Create a new session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_create(&self, request: &SessionCreateRequest) -> Result<SessionCreateResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::NewSession {
            name: request.name.clone(),
        })? {
            bmux_ipc::ResponsePayload::SessionCreated { id, name } => {
                Ok(SessionCreateResponse { id, name })
            }
            _ => Err(unexpected("session_create")),
        }
    }

    /// Select (attach to) a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_select(&self, request: &SessionSelectRequest) -> Result<SessionSelectResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::Attach {
            selector: session_selector_to_ipc(&request.selector),
        })? {
            bmux_ipc::ResponsePayload::Attached { grant } => Ok(SessionSelectResponse {
                session_id: grant.session_id,
                attach_token: grant.attach_token,
                expires_at_epoch_ms: grant.expires_at_epoch_ms,
            }),
            _ => Err(unexpected("session_select")),
        }
    }

    /// Get the current client identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn current_client(&self) -> Result<CurrentClientResponse> {
        let bmux_ipc::ResponsePayload::ClientIdentity { id: client_id } =
            self.execute_kernel_request(bmux_ipc::Request::WhoAmI)?
        else {
            return Err(unexpected("whoami"));
        };
        match self.execute_kernel_request(bmux_ipc::Request::ListClients)? {
            bmux_ipc::ResponsePayload::ClientList { clients } => {
                let current = clients.into_iter().find(|c| c.id == client_id);
                Ok(CurrentClientResponse {
                    id: client_id,
                    selected_session_id: current.as_ref().and_then(|c| c.selected_session_id),
                    following_client_id: current.as_ref().and_then(|c| c.following_client_id),
                    following_global: current.as_ref().is_some_and(|c| c.following_global),
                })
            }
            _ => Err(unexpected("current_client")),
        }
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
        match self.execute_kernel_request(bmux_ipc::Request::ListPanes {
            session: request.session.as_ref().map(session_selector_to_ipc),
        })? {
            bmux_ipc::ResponsePayload::PaneList { panes } => Ok(PaneListResponse {
                panes: panes
                    .into_iter()
                    .map(|p| PaneSummary {
                        id: p.id,
                        index: p.index,
                        name: p.name,
                        focused: p.focused,
                    })
                    .collect(),
            }),
            _ => Err(unexpected("pane_list")),
        }
    }

    /// Split a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_split(&self, request: &PaneSplitRequest) -> Result<PaneSplitResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::SplitPane {
            session: request.session.as_ref().map(session_selector_to_ipc),
            target: request.target.as_ref().map(pane_selector_to_ipc),
            direction: split_direction_to_ipc(request.direction),
            ratio_pct: None,
        })? {
            bmux_ipc::ResponsePayload::PaneSplit { id, session_id } => {
                Ok(PaneSplitResponse { id, session_id })
            }
            _ => Err(unexpected("pane_split")),
        }
    }

    /// Launch a pane with explicit command metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_launch(&self, request: &PaneLaunchRequest) -> Result<PaneLaunchResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::LaunchPane {
            session: request.session.as_ref().map(session_selector_to_ipc),
            target: request.target.as_ref().map(pane_selector_to_ipc),
            direction: split_direction_to_ipc(request.direction),
            name: request.name.clone(),
            command: bmux_ipc::PaneLaunchCommand {
                program: request.command.program.clone(),
                args: request.command.args.clone(),
                cwd: request.command.cwd.clone(),
                env: request.command.env.clone(),
            },
        })? {
            bmux_ipc::ResponsePayload::PaneLaunched { id, session_id } => {
                Ok(PaneLaunchResponse { id, session_id })
            }
            _ => Err(unexpected("pane_launch")),
        }
    }

    /// Focus a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_focus(&self, request: &PaneFocusRequest) -> Result<PaneFocusResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::FocusPane {
            session: request.session.as_ref().map(session_selector_to_ipc),
            target: request.target.as_ref().map(pane_selector_to_ipc),
            direction: request.direction.map(focus_direction_to_ipc),
        })? {
            bmux_ipc::ResponsePayload::PaneFocused { id, session_id } => {
                Ok(PaneFocusResponse { id, session_id })
            }
            _ => Err(unexpected("pane_focus")),
        }
    }

    /// Resize a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_resize(&self, request: &PaneResizeRequest) -> Result<PaneResizeResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::ResizePane {
            session: request.session.as_ref().map(session_selector_to_ipc),
            target: request.target.as_ref().map(pane_selector_to_ipc),
            delta: request.delta,
        })? {
            bmux_ipc::ResponsePayload::PaneResized { session_id } => {
                Ok(PaneResizeResponse { session_id })
            }
            _ => Err(unexpected("pane_resize")),
        }
    }

    /// Close a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::ClosePane {
            session: request.session.as_ref().map(session_selector_to_ipc),
            target: request.target.as_ref().map(pane_selector_to_ipc),
        })? {
            bmux_ipc::ResponsePayload::PaneClosed {
                id,
                session_id,
                session_closed,
            } => Ok(PaneCloseResponse {
                id,
                session_id,
                session_closed,
            }),
            _ => Err(unexpected("pane_close")),
        }
    }

    /// Toggle the zoom state of the currently-active pane in the
    /// targeted session (or the selected session when none is given).
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_zoom(&self, request: &PaneZoomRequest) -> Result<PaneZoomResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::ZoomPane {
            session: request.session.as_ref().map(session_selector_to_ipc),
        })? {
            bmux_ipc::ResponsePayload::PaneZoomed {
                session_id,
                pane_id,
                zoomed,
            } => Ok(PaneZoomResponse {
                session_id,
                pane_id,
                zoomed,
            }),
            _ => Err(unexpected("pane_zoom")),
        }
    }
}

impl<T: ServiceCaller + ?Sized> DomainCompat for T {}
