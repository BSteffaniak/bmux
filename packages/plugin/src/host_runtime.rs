//! Host-side service caller trait and `HostRuntimeApi`.
//!
//! These traits require the host-side service dispatch implementation
//! (provided via `bmux_ipc`) and are therefore defined here rather than
//! in the SDK.

use bmux_plugin_sdk::{
    ContextCloseRequest, ContextCloseResponse, ContextCreateRequest, ContextCreateResponse,
    ContextCurrentResponse, ContextListResponse, ContextSelectRequest, ContextSelectResponse,
    CurrentClientResponse, LogWriteRequest, PaneCloseRequest, PaneCloseResponse, PaneFocusRequest,
    PaneFocusResponse, PaneListRequest, PaneListResponse, PaneResizeRequest, PaneResizeResponse,
    PaneSplitRequest, PaneSplitResponse, RecordingWriteEventRequest, RecordingWriteEventResponse,
    Result, ServiceKind, SessionCreateRequest, SessionCreateResponse, SessionKillRequest,
    SessionKillResponse, SessionListResponse, SessionSelectRequest, SessionSelectResponse,
    StorageGetRequest, StorageGetResponse, StorageSetRequest,
};
use serde::{Serialize, de::DeserializeOwned};

/// Trait for types that can dispatch cross-plugin service calls.
///
/// The three context types ([`NativeCommandContext`], [`NativeLifecycleContext`],
/// [`NativeServiceContext`]) implement this trait.  The higher-level
/// [`HostRuntimeApi`] is a blanket impl over `ServiceCaller`, providing
/// ergonomic methods like `session_list()`.
pub trait ServiceCaller {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>>;

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
}

pub trait HostRuntimeApi: ServiceCaller {
    fn session_list(&self) -> Result<SessionListResponse> {
        self.call_service(
            "bmux.sessions.read",
            ServiceKind::Query,
            "session-query/v1",
            "list",
            &(),
        )
    }

    fn session_create(&self, request: &SessionCreateRequest) -> Result<SessionCreateResponse> {
        self.call_service(
            "bmux.sessions.write",
            ServiceKind::Command,
            "session-command/v1",
            "new",
            request,
        )
    }

    fn session_kill(&self, request: &SessionKillRequest) -> Result<SessionKillResponse> {
        self.call_service(
            "bmux.sessions.write",
            ServiceKind::Command,
            "session-command/v1",
            "kill",
            request,
        )
    }

    fn session_select(&self, request: &SessionSelectRequest) -> Result<SessionSelectResponse> {
        self.call_service(
            "bmux.sessions.write",
            ServiceKind::Command,
            "session-command/v1",
            "select",
            request,
        )
    }

    fn current_client(&self) -> Result<CurrentClientResponse> {
        self.call_service(
            "bmux.clients.read",
            ServiceKind::Query,
            "client-query/v1",
            "current",
            &(),
        )
    }

    fn context_list(&self) -> Result<ContextListResponse> {
        self.call_service(
            "bmux.contexts.read",
            ServiceKind::Query,
            "context-query/v1",
            "list",
            &(),
        )
    }

    fn context_current(&self) -> Result<ContextCurrentResponse> {
        self.call_service(
            "bmux.contexts.read",
            ServiceKind::Query,
            "context-query/v1",
            "current",
            &(),
        )
    }

    fn context_create(&self, request: &ContextCreateRequest) -> Result<ContextCreateResponse> {
        self.call_service(
            "bmux.contexts.write",
            ServiceKind::Command,
            "context-command/v1",
            "create",
            request,
        )
    }

    fn context_select(&self, request: &ContextSelectRequest) -> Result<ContextSelectResponse> {
        self.call_service(
            "bmux.contexts.write",
            ServiceKind::Command,
            "context-command/v1",
            "select",
            request,
        )
    }

    fn context_close(&self, request: &ContextCloseRequest) -> Result<ContextCloseResponse> {
        self.call_service(
            "bmux.contexts.write",
            ServiceKind::Command,
            "context-command/v1",
            "close",
            request,
        )
    }

    fn pane_list(&self, request: &PaneListRequest) -> Result<PaneListResponse> {
        self.call_service(
            "bmux.panes.read",
            ServiceKind::Query,
            "pane-query/v1",
            "list",
            request,
        )
    }

    fn pane_split(&self, request: &PaneSplitRequest) -> Result<PaneSplitResponse> {
        self.call_service(
            "bmux.panes.write",
            ServiceKind::Command,
            "pane-command/v1",
            "split",
            request,
        )
    }

    fn pane_focus(&self, request: &PaneFocusRequest) -> Result<PaneFocusResponse> {
        self.call_service(
            "bmux.panes.write",
            ServiceKind::Command,
            "pane-command/v1",
            "focus",
            request,
        )
    }

    fn pane_resize(&self, request: &PaneResizeRequest) -> Result<PaneResizeResponse> {
        self.call_service(
            "bmux.panes.write",
            ServiceKind::Command,
            "pane-command/v1",
            "resize",
            request,
        )
    }

    fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse> {
        self.call_service(
            "bmux.panes.write",
            ServiceKind::Command,
            "pane-command/v1",
            "close",
            request,
        )
    }

    fn storage_get(&self, request: &StorageGetRequest) -> Result<StorageGetResponse> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Query,
            "storage-query/v1",
            "get",
            request,
        )
    }

    fn storage_set(&self, request: &StorageSetRequest) -> Result<()> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Command,
            "storage-command/v1",
            "set",
            request,
        )
    }

    fn log_write(&self, request: &LogWriteRequest) -> Result<()> {
        self.call_service(
            "bmux.logs.write",
            ServiceKind::Command,
            "logging-command/v1",
            "write",
            request,
        )
    }

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
