//! Host-side service caller trait and `HostRuntimeApi`.
//!
//! These traits require the host-side service dispatch implementation
//! (provided via `bmux_ipc`) and are therefore defined here rather than
//! in the SDK.

use bmux_plugin_sdk::{
    CORE_CLI_COMMAND_CAPABILITY, CORE_CLI_COMMAND_INTERFACE_V1,
    CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1, CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
    ContextCloseRequest, ContextCloseResponse, ContextCreateRequest, ContextCreateResponse,
    ContextCurrentResponse, ContextListResponse, ContextSelectRequest, ContextSelectResponse,
    CoreCliCommandRequest, CoreCliCommandResponse, CurrentClientResponse, LogWriteRequest,
    PaneCloseRequest, PaneCloseResponse, PaneFocusRequest, PaneFocusResponse, PaneLaunchRequest,
    PaneLaunchResponse, PaneListRequest, PaneListResponse, PaneResizeRequest, PaneResizeResponse,
    PaneSplitRequest, PaneSplitResponse, PaneZoomRequest, PaneZoomResponse,
    PluginCliCommandRequest, PluginCliCommandResponse, RecordingWriteEventRequest,
    RecordingWriteEventResponse, Result, ServiceKind, SessionCreateRequest, SessionCreateResponse,
    SessionKillRequest, SessionKillResponse, SessionListResponse, SessionSelectRequest,
    SessionSelectResponse, StorageGetRequest, StorageGetResponse, StorageSetRequest,
};
use serde::{Serialize, de::DeserializeOwned};

/// Trait for types that can dispatch cross-plugin service calls.
///
/// The three context types ([`NativeCommandContext`], [`NativeLifecycleContext`],
/// [`NativeServiceContext`]) implement this trait.  The higher-level
/// [`HostRuntimeApi`] is a blanket impl over `ServiceCaller`, providing
/// ergonomic methods like `session_list()`.
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
                    .map(|entry| bmux_plugin_sdk::SessionSummary {
                        id: entry.id,
                        name: entry.name,
                        client_count: entry.client_count,
                    })
                    .collect(),
            }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for session_list".to_string(),
            }),
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
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for session_create".to_string(),
            }),
        }
    }

    /// Kill a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_kill(&self, request: &SessionKillRequest) -> Result<SessionKillResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::KillSession {
            selector: host_session_selector_to_ipc(&request.selector),
            force_local: request.force_local,
        })? {
            bmux_ipc::ResponsePayload::SessionKilled { id } => Ok(SessionKillResponse { id }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for session_kill".to_string(),
            }),
        }
    }

    /// Select (attach to) a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn session_select(&self, request: &SessionSelectRequest) -> Result<SessionSelectResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::Attach {
            selector: host_session_selector_to_ipc(&request.selector),
        })? {
            bmux_ipc::ResponsePayload::Attached { grant } => Ok(SessionSelectResponse {
                session_id: grant.session_id,
                attach_token: grant.attach_token,
                expires_at_epoch_ms: grant.expires_at_epoch_ms,
            }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for session_select".to_string(),
            }),
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
            return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for whoami".to_string(),
            });
        };
        match self.execute_kernel_request(bmux_ipc::Request::ListClients)? {
            bmux_ipc::ResponsePayload::ClientList { clients } => {
                let current = clients.into_iter().find(|entry| entry.id == client_id);
                Ok(CurrentClientResponse {
                    id: client_id,
                    selected_session_id: current
                        .as_ref()
                        .and_then(|entry| entry.selected_session_id),
                    following_client_id: current
                        .as_ref()
                        .and_then(|entry| entry.following_client_id),
                    following_global: current.as_ref().is_some_and(|entry| entry.following_global),
                })
            }
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for current_client".to_string(),
            }),
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
                    .map(|entry| bmux_plugin_sdk::ContextSummary {
                        id: entry.id,
                        name: entry.name,
                        attributes: entry.attributes,
                    })
                    .collect(),
            }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for context_list".to_string(),
            }),
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
                context: context.map(|entry| bmux_plugin_sdk::ContextSummary {
                    id: entry.id,
                    name: entry.name,
                    attributes: entry.attributes,
                }),
            }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for context_current".to_string(),
            }),
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
                context: bmux_plugin_sdk::ContextSummary {
                    id: context.id,
                    name: context.name,
                    attributes: context.attributes,
                },
            }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for context_create".to_string(),
            }),
        }
    }

    /// Select (switch to) a context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_select(&self, request: &ContextSelectRequest) -> Result<ContextSelectResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::SelectContext {
            selector: host_context_selector_to_ipc(&request.selector),
        })? {
            bmux_ipc::ResponsePayload::ContextSelected { context } => Ok(ContextSelectResponse {
                context: bmux_plugin_sdk::ContextSummary {
                    id: context.id,
                    name: context.name,
                    attributes: context.attributes,
                },
            }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for context_select".to_string(),
            }),
        }
    }

    /// Close a context.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn context_close(&self, request: &ContextCloseRequest) -> Result<ContextCloseResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::CloseContext {
            selector: host_context_selector_to_ipc(&request.selector),
            force: request.force,
        })? {
            bmux_ipc::ResponsePayload::ContextClosed { id } => Ok(ContextCloseResponse { id }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for context_close".to_string(),
            }),
        }
    }

    /// List panes.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_list(&self, request: &PaneListRequest) -> Result<PaneListResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::ListPanes {
            session: request.session.as_ref().map(host_session_selector_to_ipc),
        })? {
            bmux_ipc::ResponsePayload::PaneList { panes } => Ok(PaneListResponse {
                panes: panes
                    .into_iter()
                    .map(|entry| bmux_plugin_sdk::PaneSummary {
                        id: entry.id,
                        index: entry.index,
                        name: entry.name,
                        focused: entry.focused,
                    })
                    .collect(),
            }),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for pane_list".to_string(),
            }),
        }
    }

    /// Split a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_split(&self, request: &PaneSplitRequest) -> Result<PaneSplitResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::SplitPane {
            session: request.session.as_ref().map(host_session_selector_to_ipc),
            target: request.target.as_ref().map(host_pane_selector_to_ipc),
            direction: host_split_direction_to_ipc(request.direction),
            ratio_pct: None,
        })? {
            bmux_ipc::ResponsePayload::PaneSplit { id, session_id } => {
                Ok(PaneSplitResponse { id, session_id })
            }
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for pane_split".to_string(),
            }),
        }
    }

    /// Launch a pane with explicit command metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_launch(&self, request: &PaneLaunchRequest) -> Result<PaneLaunchResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::LaunchPane {
            session: request.session.as_ref().map(host_session_selector_to_ipc),
            target: request.target.as_ref().map(host_pane_selector_to_ipc),
            direction: host_split_direction_to_ipc(request.direction),
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
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for pane_launch".to_string(),
            }),
        }
    }

    /// Focus a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_focus(&self, request: &PaneFocusRequest) -> Result<PaneFocusResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::FocusPane {
            session: request.session.as_ref().map(host_session_selector_to_ipc),
            target: request.target.as_ref().map(host_pane_selector_to_ipc),
            direction: request.direction.map(host_focus_direction_to_ipc),
        })? {
            bmux_ipc::ResponsePayload::PaneFocused { id, session_id } => {
                Ok(PaneFocusResponse { id, session_id })
            }
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for pane_focus".to_string(),
            }),
        }
    }

    /// Resize a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_resize(&self, request: &PaneResizeRequest) -> Result<PaneResizeResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::ResizePane {
            session: request.session.as_ref().map(host_session_selector_to_ipc),
            target: request.target.as_ref().map(host_pane_selector_to_ipc),
            delta: request.delta,
        })? {
            bmux_ipc::ResponsePayload::PaneResized { session_id } => {
                Ok(PaneResizeResponse { session_id })
            }
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for pane_resize".to_string(),
            }),
        }
    }

    /// Close a pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse> {
        match self.execute_kernel_request(bmux_ipc::Request::ClosePane {
            session: request.session.as_ref().map(host_session_selector_to_ipc),
            target: request.target.as_ref().map(host_pane_selector_to_ipc),
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
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for pane_close".to_string(),
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
        match self.execute_kernel_request(bmux_ipc::Request::ZoomPane {
            session: request.session.as_ref().map(host_session_selector_to_ipc),
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
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "unexpected response payload for pane_zoom".to_string(),
            }),
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

// ── SDK → IPC converters ────────────────────────────────────────────

fn host_session_selector_to_ipc(
    selector: &bmux_plugin_sdk::SessionSelector,
) -> bmux_ipc::SessionSelector {
    match selector {
        bmux_plugin_sdk::SessionSelector::ById(id) => bmux_ipc::SessionSelector::ById(*id),
        bmux_plugin_sdk::SessionSelector::ByName(name) => {
            bmux_ipc::SessionSelector::ByName(name.clone())
        }
    }
}

fn host_context_selector_to_ipc(
    selector: &bmux_plugin_sdk::ContextSelector,
) -> bmux_ipc::ContextSelector {
    match selector {
        bmux_plugin_sdk::ContextSelector::ById(id) => bmux_ipc::ContextSelector::ById(*id),
        bmux_plugin_sdk::ContextSelector::ByName(name) => {
            bmux_ipc::ContextSelector::ByName(name.clone())
        }
    }
}

const fn host_pane_selector_to_ipc(
    selector: &bmux_plugin_sdk::PaneSelector,
) -> bmux_ipc::PaneSelector {
    match selector {
        bmux_plugin_sdk::PaneSelector::ById(id) => bmux_ipc::PaneSelector::ById(*id),
        bmux_plugin_sdk::PaneSelector::ByIndex(index) => bmux_ipc::PaneSelector::ByIndex(*index),
        bmux_plugin_sdk::PaneSelector::Active => bmux_ipc::PaneSelector::Active,
    }
}

const fn host_split_direction_to_ipc(
    direction: bmux_plugin_sdk::PaneSplitDirection,
) -> bmux_ipc::PaneSplitDirection {
    match direction {
        bmux_plugin_sdk::PaneSplitDirection::Vertical => bmux_ipc::PaneSplitDirection::Vertical,
        bmux_plugin_sdk::PaneSplitDirection::Horizontal => bmux_ipc::PaneSplitDirection::Horizontal,
    }
}

const fn host_focus_direction_to_ipc(
    direction: bmux_plugin_sdk::PaneFocusDirection,
) -> bmux_ipc::PaneFocusDirection {
    match direction {
        bmux_plugin_sdk::PaneFocusDirection::Next => bmux_ipc::PaneFocusDirection::Next,
        bmux_plugin_sdk::PaneFocusDirection::Prev => bmux_ipc::PaneFocusDirection::Prev,
    }
}
