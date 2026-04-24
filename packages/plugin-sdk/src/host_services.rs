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
    /// Error message from the plugin command's `Err` return, if any.
    ///
    /// Populated by the SDK's FFI boundary when a `RustPlugin`
    /// command returns `Err(PluginCommandError)`. Hosts use this to
    /// log the error and render a user-facing indicator, instead of
    /// relying on the plugin's stderr (which would corrupt attach
    /// TTYs for in-process plugins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}
