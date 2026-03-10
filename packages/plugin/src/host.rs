use crate::{HostScope, PluginEvent, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use toml::Value;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetadata {
    pub product_name: String,
    pub product_version: String,
    pub plugin_api_version: crate::ApiVersion,
    pub plugin_abi_version: crate::ApiVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionHandle(pub Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WindowHandle(pub Uuid);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PaneHandle(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConnectionInfo {
    pub config_dir: String,
    pub runtime_dir: String,
    pub data_dir: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRoleValue {
    Owner,
    Writer,
    Observer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub handle: SessionHandle,
    pub name: Option<String>,
    pub window_count: usize,
    pub client_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub handle: WindowHandle,
    pub session: SessionHandle,
    pub number: u32,
    pub name: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub handle: PaneHandle,
    pub session: SessionHandle,
    pub window: WindowHandle,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub pane: PaneSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneLayoutNode {
    Leaf {
        pane: PaneHandle,
    },
    Split {
        direction: PaneSplitDirection,
        ratio_percent: u8,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    pub window: WindowSummary,
    pub focused_pane: Option<PaneHandle>,
    pub panes: Vec<PaneSummary>,
    #[serde(default)]
    pub layout_root: Option<PaneLayoutNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session: SessionSummary,
    pub active_window: Option<WindowHandle>,
    pub windows: Vec<WindowSummary>,
    pub clients: Vec<ClientSummary>,
    pub permissions: Vec<PermissionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionEntry {
    pub client_id: Uuid,
    pub role: SessionRoleValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSummary {
    pub id: Uuid,
    pub selected_session: Option<SessionHandle>,
    pub following_client_id: Option<Uuid>,
    pub following_global: bool,
    pub role: Option<SessionRoleValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowState {
    pub follower_client_id: Uuid,
    pub leader_client_id: Option<Uuid>,
    pub global: bool,
    pub selected_session: Option<SessionHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceStatus {
    pub enabled: bool,
    pub path: Option<String>,
    pub snapshot_exists: bool,
    pub last_write_epoch_ms: Option<u64>,
    pub last_restore_epoch_ms: Option<u64>,
    pub last_restore_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceRestorePreview {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceRestoreResult {
    pub sessions: usize,
    pub windows: usize,
    pub roles: usize,
    pub follows: usize,
    pub selected_sessions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalIdentityInfo {
    pub principal_id: Uuid,
    pub server_owner_principal_id: Uuid,
    pub force_local_authorized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusInfo {
    pub running: bool,
    pub principal_id: Uuid,
    pub server_owner_principal_id: Uuid,
    pub snapshot: PersistenceStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRef {
    Handle(SessionHandle),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowRef {
    Handle(WindowHandle),
    Number(u32),
    Name(String),
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneRef {
    Handle(PaneHandle),
    Index(u32),
    Active,
}

pub trait PluginHost: Send + Sync {
    fn plugin_id(&self) -> &str;
    fn metadata(&self) -> &HostMetadata;
    fn connection(&self) -> &HostConnectionInfo;
    fn required_capabilities(&self) -> &BTreeSet<HostScope>;
    fn provided_capabilities(&self) -> &BTreeSet<HostScope>;
    fn has_capability(&self, capability: &HostScope) -> bool {
        self.required_capabilities().contains(capability)
            || self.provided_capabilities().contains(capability)
    }
    fn events(&self) -> &dyn EventService;
    fn session_queries(&self) -> &dyn SessionQueryService;
    fn session_commands(&self) -> &dyn SessionCommandService;
    fn window_queries(&self) -> &dyn WindowQueryService;
    fn window_commands(&self) -> &dyn WindowCommandService;
    fn pane_queries(&self) -> &dyn PaneQueryService;
    fn pane_commands(&self) -> &dyn PaneCommandService;
    fn permission_queries(&self) -> &dyn PermissionQueryService;
    fn permission_commands(&self) -> &dyn PermissionCommandService;
    fn client_queries(&self) -> &dyn ClientQueryService;
    fn follow_queries(&self) -> &dyn FollowQueryService;
    fn follow_commands(&self) -> &dyn FollowCommandService;
    fn persistence_queries(&self) -> &dyn PersistenceQueryService;
    fn persistence_commands(&self) -> &dyn PersistenceCommandService;
    fn render(&self) -> &dyn RenderService;
    fn config(&self) -> &dyn ConfigService;
    fn storage(&self) -> &dyn PluginStorage;
    fn clipboard(&self) -> &dyn ClipboardService;
}

pub struct PluginContext<'a> {
    pub host: &'a dyn PluginHost,
    pub settings: &'a BTreeMap<String, Value>,
}

pub trait EventService: Send + Sync {
    fn emit(&self, event: PluginEvent) -> Result<()>;
}

pub trait SessionQueryService: Send + Sync {
    fn active_session(&self) -> Result<Option<SessionHandle>>;
    fn list_sessions(&self) -> Result<Vec<SessionSummary>>;
    fn get_session(&self, session: SessionHandle) -> Result<Option<SessionSummary>>;
    fn snapshot_session(&self, session: SessionHandle) -> Result<Option<SessionSnapshot>>;
}

pub trait SessionCommandService: Send + Sync {
    fn create_session(&self, name: Option<String>) -> Result<SessionHandle>;
    fn kill_session(&self, session: SessionRef, force_local: bool) -> Result<SessionHandle>;
}

pub trait WindowQueryService: Send + Sync {
    fn list_windows(&self, session: Option<SessionHandle>) -> Result<Vec<WindowSummary>>;
    fn get_window(&self, window: WindowHandle) -> Result<Option<WindowSummary>>;
    fn snapshot_window(&self, window: WindowHandle) -> Result<Option<WindowSnapshot>>;
}

pub trait WindowCommandService: Send + Sync {
    fn create_window(
        &self,
        session: Option<SessionHandle>,
        name: Option<String>,
    ) -> Result<WindowHandle>;
    fn kill_window(
        &self,
        session: Option<SessionHandle>,
        target: WindowRef,
        force_local: bool,
    ) -> Result<WindowHandle>;
    fn switch_window(
        &self,
        session: Option<SessionHandle>,
        target: WindowRef,
    ) -> Result<WindowHandle>;
}

pub trait PaneQueryService: Send + Sync {
    fn focused_pane(&self, session: Option<SessionHandle>) -> Result<Option<PaneHandle>>;
    fn list_panes(&self, session: Option<SessionHandle>) -> Result<Vec<PaneSummary>>;
    fn get_pane(&self, pane: PaneHandle) -> Result<Option<PaneSummary>>;
    fn snapshot_pane(&self, pane: PaneHandle) -> Result<Option<PaneSnapshot>>;
}

pub trait PaneCommandService: Send + Sync {
    fn split_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        direction: PaneSplitDirection,
    ) -> Result<PaneHandle>;
    fn focus_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        direction: Option<PaneFocusDirection>,
    ) -> Result<PaneHandle>;
    fn resize_pane(
        &self,
        session: Option<SessionHandle>,
        target: Option<PaneRef>,
        delta: i16,
    ) -> Result<()>;
    fn close_pane(&self, session: Option<SessionHandle>, target: Option<PaneRef>) -> Result<()>;
}

pub trait PermissionQueryService: Send + Sync {
    fn list_permissions(&self, session: SessionHandle) -> Result<Vec<PermissionEntry>>;
}

pub trait PermissionCommandService: Send + Sync {
    fn grant_role(
        &self,
        session: SessionHandle,
        client_id: Uuid,
        role: SessionRoleValue,
    ) -> Result<()>;
    fn revoke_role(&self, session: SessionHandle, client_id: Uuid) -> Result<()>;
}

pub trait ClientQueryService: Send + Sync {
    fn current_client_id(&self) -> Result<Uuid>;
    fn principal_identity(&self) -> Result<PrincipalIdentityInfo>;
    fn list_clients(&self) -> Result<Vec<ClientSummary>>;
}

pub trait FollowQueryService: Send + Sync {
    fn current_follow_state(&self) -> Result<FollowState>;
}

pub trait FollowCommandService: Send + Sync {
    fn follow_client(&self, target_client_id: Uuid, global: bool) -> Result<()>;
    fn unfollow(&self) -> Result<()>;
}

pub trait PersistenceQueryService: Send + Sync {
    fn status(&self) -> Result<PersistenceStatus>;
    fn server_status(&self) -> Result<ServerStatusInfo>;
}

pub trait PersistenceCommandService: Send + Sync {
    fn save(&self) -> Result<Option<String>>;
    fn restore_dry_run(&self) -> Result<PersistenceRestorePreview>;
    fn restore_apply(&self) -> Result<PersistenceRestoreResult>;
}

pub trait RenderService: Send + Sync {
    fn invalidate(&self) -> Result<()>;
}

pub trait ConfigService: Send + Sync {
    fn plugin_settings(&self, plugin_id: &str) -> Result<BTreeMap<String, Value>>;
}

pub trait PluginStorage: Send + Sync {
    fn get(&self, plugin_id: &str, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, plugin_id: &str, key: &str, value: Vec<u8>) -> Result<()>;
}

pub trait ClipboardService: Send + Sync {
    fn copy_text(&self, text: &str) -> Result<()>;
}
