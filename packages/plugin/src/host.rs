use crate::{PluginEvent, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub handle: SessionHandle,
    pub name: Option<String>,
    pub window_count: usize,
    pub client_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub handle: WindowHandle,
    pub session: SessionHandle,
    pub number: u32,
    pub name: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub handle: PaneHandle,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionEntry {
    pub client_id: Uuid,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: Uuid,
    pub selected_session: Option<SessionHandle>,
    pub following_client_id: Option<Uuid>,
    pub following_global: bool,
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusInfo {
    pub running: bool,
    pub principal_id: Uuid,
    pub server_owner_principal_id: Uuid,
    pub snapshot_enabled: bool,
    pub snapshot_exists: bool,
}

pub trait PluginHost: Send + Sync {
    fn metadata(&self) -> &HostMetadata;
    fn connection(&self) -> &HostConnectionInfo;
    fn events(&self) -> &dyn EventService;
    fn commands(&self) -> &dyn CommandService;
    fn sessions(&self) -> &dyn SessionService;
    fn windows(&self) -> &dyn WindowService;
    fn panes(&self) -> &dyn PaneService;
    fn permissions(&self) -> &dyn PermissionService;
    fn clients(&self) -> &dyn ClientService;
    fn server(&self) -> &dyn ServerService;
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

pub trait CommandService: Send + Sync {
    fn invoke(&self, command_name: &str, arguments: &[String]) -> Result<()>;
}

pub trait SessionService: Send + Sync {
    fn active_session(&self) -> Result<Option<SessionHandle>>;
    fn list_sessions(&self) -> Result<Vec<SessionHandle>>;
    fn get_session(&self, session: SessionHandle) -> Result<Option<SessionInfo>>;
}

pub trait WindowService: Send + Sync {
    fn list_windows(&self, session: SessionHandle) -> Result<Vec<WindowHandle>>;
    fn get_window(&self, window: WindowHandle) -> Result<Option<WindowInfo>>;
}

pub trait PaneService: Send + Sync {
    fn focused_pane(&self) -> Result<Option<PaneHandle>>;
    fn list_panes(&self, window: WindowHandle) -> Result<Vec<PaneHandle>>;
    fn get_pane(&self, pane: PaneHandle) -> Result<Option<PaneInfo>>;
}

pub trait PermissionService: Send + Sync {
    fn list_permissions(&self, session: SessionHandle) -> Result<Vec<PermissionEntry>>;
}

pub trait ClientService: Send + Sync {
    fn list_clients(&self) -> Result<Vec<ClientInfo>>;
}

pub trait ServerService: Send + Sync {
    fn status(&self) -> Result<ServerStatusInfo>;
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
