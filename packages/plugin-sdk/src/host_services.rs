use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogWriteLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogWriteRequest {
    pub level: LogWriteLevel,
    pub message: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingWriteEventRequest {
    pub session_id: Option<Uuid>,
    pub pane_id: Option<Uuid>,
    pub name: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingWriteEventResponse {
    pub accepted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandOutcome {
    // Intentionally empty. Previously carried a `Vec<PluginCommandEffect>`
    // with a single `SelectContext` variant used to retarget the attach
    // view after a context-selection command. In M4 Stage 7 that
    // side-channel was deleted: cross-domain mutations now go through
    // typed dispatch (the contexts plugin's `select-context` command
    // sets the context directly) and the attach runtime retargets
    // based on observing the `before/after current-context` delta
    // rather than a plugin-emitted effect list. Kept as an empty
    // struct so `PluginCommandExecution::outcome` retains a stable
    // shape; drop this type in a future milestone once no consumer
    // references it.
}
