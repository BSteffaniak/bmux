//! Host-side service caller trait and `HostRuntimeApi`.
//!
//! These traits require the host-side service dispatch implementation
//! (provided via `bmux_ipc`) and are therefore defined here rather than
//! in the SDK.
//!
//! The domain-specific `HostRuntimeApi` convenience methods
//! (`session_*`, `context_*`, `pane_*`, `current_client`) are
//! implemented on top of [`ServiceCaller::execute_kernel_request`] —
//! core no longer ships fake typed service interfaces
//! (`session-query/v1` etc.) for them. The methods persist for
//! plugin-author ergonomics while the windows/permissions/cluster
//! plugins migrate to using their typed plugin-api contracts or
//! `execute_kernel_request` directly. See `.m4-scratch.md` for the
//! remaining migration checklist.

use bmux_plugin_sdk::{
    CORE_CLI_COMMAND_CAPABILITY, CORE_CLI_COMMAND_INTERFACE_V1,
    CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1, CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
    ContextCloseRequest, ContextCloseResponse, ContextCreateRequest, ContextCreateResponse,
    ContextCurrentResponse, ContextListResponse, ContextSelectRequest, ContextSelectResponse,
    ContextSelector, ContextSummary, CoreCliCommandRequest, CoreCliCommandResponse,
    CurrentClientResponse, LogWriteRequest, PaneCloseRequest, PaneCloseResponse,
    PaneFocusDirection, PaneFocusRequest, PaneFocusResponse, PaneLaunchRequest, PaneLaunchResponse,
    PaneListRequest, PaneListResponse, PaneResizeRequest, PaneResizeResponse, PaneSelector,
    PaneSplitDirection, PaneSplitRequest, PaneSplitResponse, PaneSummary, PaneZoomRequest,
    PaneZoomResponse, PluginCliCommandRequest, PluginCliCommandResponse, PluginError,
    RecordingWriteEventRequest, RecordingWriteEventResponse, Result, ServiceKind,
    SessionCreateRequest, SessionCreateResponse, SessionKillRequest, SessionKillResponse,
    SessionListResponse, SessionSelectRequest, SessionSelectResponse, SessionSelector,
    SessionSummary, StorageGetRequest, StorageGetResponse, StorageSetRequest,
};
use serde::{Serialize, de::DeserializeOwned};

/// Trait for types that can dispatch cross-plugin service calls.
///
/// The three context types ([`NativeCommandContext`](crate::NativeCommandContext),
/// [`NativeLifecycleContext`](crate::NativeLifecycleContext),
/// [`NativeServiceContext`](crate::NativeServiceContext)) and the
/// long-lived [`TypedServiceCaller`](crate::TypedServiceCaller)
/// implement this trait.
pub trait ServiceCaller {
    /// Dispatch a raw service call with a binary payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability is not accessible, the service
    /// is not registered, or the provider returns a transport-level error.
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>>;

    /// Dispatch a typed service call, serializing the request and
    /// deserializing the response.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability is not accessible, the service
    /// is not registered, or the provider returns a transport-level error.
    fn call_service<Request, Response>(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Serialize,
        Response: DeserializeOwned,
    {
        let payload = bmux_plugin_sdk::encode_service_message(request)?;
        let response = self.call_service_raw(capability, kind, interface_id, operation, payload)?;
        bmux_plugin_sdk::decode_service_message(&response)
    }

    /// Dispatch an IPC request directly to the host kernel.
    ///
    /// Used by domain plugins (sessions, contexts, clients, windows)
    /// that own typed services backed by core state the host kernel
    /// exposes over IPC. The typed service layer would otherwise cycle
    /// back into the plugin's own handlers.
    ///
    /// # Errors
    ///
    /// Returns an error when the host kernel bridge is missing
    /// (typical in tests), or the server returns an error response.
    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload>;
}

// ── SDK → IPC selector converters (used by HostRuntimeApi defaults) ─

fn session_selector_to_ipc(selector: &SessionSelector) -> bmux_ipc::SessionSelector {
    match selector {
        SessionSelector::ById(id) => bmux_ipc::SessionSelector::ById(*id),
        SessionSelector::ByName(name) => bmux_ipc::SessionSelector::ByName(name.clone()),
    }
}

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

pub trait HostRuntimeApi: ServiceCaller {
    /// Run a core built-in CLI command path in-process.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn core_cli_command_run_path(
        &self,
        request: &CoreCliCommandRequest,
    ) -> Result<CoreCliCommandResponse> {
        self.call_service(
            CORE_CLI_COMMAND_CAPABILITY,
            ServiceKind::Command,
            CORE_CLI_COMMAND_INTERFACE_V1,
            CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
            request,
        )
    }

    /// Run a plugin command in-process.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn plugin_command_run(
        &self,
        request: &PluginCliCommandRequest,
    ) -> Result<PluginCliCommandResponse> {
        self.call_service(
            CORE_CLI_COMMAND_CAPABILITY,
            ServiceKind::Command,
            CORE_CLI_COMMAND_INTERFACE_V1,
            CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
            request,
        )
    }

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

    /// Kill a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_kill(&self, request: &SessionKillRequest) -> Result<SessionKillResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::KillSession {
            selector: session_selector_to_ipc(&request.selector),
            force_local: request.force_local,
        })? {
            bmux_ipc::ResponsePayload::SessionKilled { id } => Ok(SessionKillResponse { id }),
            _ => Err(unexpected("session_kill")),
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
        match self.execute_kernel_request(bmux_ipc::Request::ListContexts)? {
            bmux_ipc::ResponsePayload::ContextList { contexts } => Ok(ContextListResponse {
                contexts: contexts
                    .into_iter()
                    .map(|c| ContextSummary {
                        id: c.id,
                        name: c.name,
                        attributes: c.attributes,
                    })
                    .collect(),
            }),
            _ => Err(unexpected("context_list")),
        }
    }

    /// Get the current context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_current(&self) -> Result<ContextCurrentResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::CurrentContext)? {
            bmux_ipc::ResponsePayload::CurrentContext { context } => Ok(ContextCurrentResponse {
                context: context.map(|c| ContextSummary {
                    id: c.id,
                    name: c.name,
                    attributes: c.attributes,
                }),
            }),
            _ => Err(unexpected("context_current")),
        }
    }

    /// Create a new context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_create(&self, request: &ContextCreateRequest) -> Result<ContextCreateResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::CreateContext {
            name: request.name.clone(),
            attributes: request.attributes.clone(),
        })? {
            bmux_ipc::ResponsePayload::ContextCreated { context } => Ok(ContextCreateResponse {
                context: ContextSummary {
                    id: context.id,
                    name: context.name,
                    attributes: context.attributes,
                },
            }),
            _ => Err(unexpected("context_create")),
        }
    }

    /// Select (switch to) a context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_select(&self, request: &ContextSelectRequest) -> Result<ContextSelectResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::SelectContext {
            selector: context_selector_to_ipc(&request.selector),
        })? {
            bmux_ipc::ResponsePayload::ContextSelected { context } => Ok(ContextSelectResponse {
                context: ContextSummary {
                    id: context.id,
                    name: context.name,
                    attributes: context.attributes,
                },
            }),
            _ => Err(unexpected("context_select")),
        }
    }

    /// Close a context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_close(&self, request: &ContextCloseRequest) -> Result<ContextCloseResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::CloseContext {
            selector: context_selector_to_ipc(&request.selector),
            force: request.force,
        })? {
            bmux_ipc::ResponsePayload::ContextClosed { id } => Ok(ContextCloseResponse { id }),
            _ => Err(unexpected("context_close")),
        }
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

    /// Get a value from plugin storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn storage_get(&self, request: &StorageGetRequest) -> Result<StorageGetResponse> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Query,
            "storage-query/v1",
            "get",
            request,
        )
    }

    /// Set a value in plugin storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn storage_set(&self, request: &StorageSetRequest) -> Result<()> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Command,
            "storage-command/v1",
            "set",
            request,
        )
    }

    /// Write a log message.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn log_write(&self, request: &LogWriteRequest) -> Result<()> {
        self.call_service(
            "bmux.logs.write",
            ServiceKind::Command,
            "logging-command/v1",
            "write",
            request,
        )
    }

    /// Write a custom recording event.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn recording_write_event(
        &self,
        request: &RecordingWriteEventRequest,
    ) -> Result<RecordingWriteEventResponse> {
        self.call_service(
            "bmux.recording.write",
            ServiceKind::Command,
            "recording-command/v1",
            "write_event",
            request,
        )
    }
}

impl<T> HostRuntimeApi for T where T: ServiceCaller + ?Sized {}
