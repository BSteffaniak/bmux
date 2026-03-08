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

pub trait PluginHost: Send + Sync {
    fn metadata(&self) -> &HostMetadata;
    fn events(&self) -> &dyn EventService;
    fn commands(&self) -> &dyn CommandService;
    fn sessions(&self) -> &dyn SessionService;
    fn windows(&self) -> &dyn WindowService;
    fn panes(&self) -> &dyn PaneService;
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
}

pub trait WindowService: Send + Sync {
    fn list_windows(&self, session: SessionHandle) -> Result<Vec<WindowHandle>>;
}

pub trait PaneService: Send + Sync {
    fn focused_pane(&self) -> Result<Option<PaneHandle>>;
    fn list_panes(&self, window: WindowHandle) -> Result<Vec<PaneHandle>>;
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
