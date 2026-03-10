use crate::{HostScope, PluginError, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Query,
    Command,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginService {
    pub capability: HostScope,
    pub kind: ServiceKind,
    pub interface_id: String,
}

impl PluginService {
    pub fn validate(&self, plugin_id: &str) -> Result<()> {
        if self.interface_id.trim().is_empty() {
            return Err(PluginError::InvalidServiceInterfaceId {
                plugin_id: plugin_id.to_string(),
                capability: self.capability.as_str().to_string(),
                kind: self.kind,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredService {
    pub capability: HostScope,
    pub kind: ServiceKind,
    pub interface_id: String,
    pub provider_plugin_id: String,
}

impl RegisteredService {
    #[must_use]
    pub fn key(&self) -> (HostScope, ServiceKind, String) {
        (
            self.capability.clone(),
            self.kind,
            self.interface_id.clone(),
        )
    }
}
