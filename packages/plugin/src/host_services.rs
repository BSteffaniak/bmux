use crate::{Result, ServiceCaller, ServiceKind};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    pub attributes: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub id: Uuid,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
}

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
pub struct SessionKillRequest {
    pub selector: SessionSelector,
    pub force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKillResponse {
    pub id: Uuid,
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
    pub attributes: std::collections::BTreeMap<String, String>,
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
pub struct PaneSplitResponse {
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
pub struct StorageGetRequest {
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageGetResponse {
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSetRequest {
    pub key: String,
    pub value: Vec<u8>,
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
}

impl<T> HostRuntimeApi for T where T: ServiceCaller + ?Sized {}
