use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginEventKind {
    System,
    Session,
    Window,
    Pane,
    Client,
    Command,
    Terminal,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginEvent {
    pub kind: PluginEventKind,
    pub name: String,
    #[serde(default)]
    pub payload: PluginEventPayload,
}

pub type PluginEventPayload = Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEventSubscription {
    #[serde(default)]
    pub kinds: BTreeSet<PluginEventKind>,
    #[serde(default)]
    pub names: BTreeSet<String>,
}

impl PluginEventSubscription {
    #[must_use]
    pub fn matches(&self, event: &PluginEvent) -> bool {
        let kind_matches = self.kinds.is_empty() || self.kinds.contains(&event.kind);
        let name_matches = self.names.is_empty() || self.names.contains(&event.name);
        kind_matches && name_matches
    }
}
