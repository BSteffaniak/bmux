use std::{cell::RefCell, collections::BTreeMap};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateGetRequest {
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateGetResponse {
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateSetRequest {
    pub key: String,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateClearRequest {
    pub key: String,
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
    /// Generic metadata produced by a plugin command.
    ///
    /// Hosts may interpret well-known keys for their own runtime surfaces while
    /// the SDK remains domain-agnostic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

thread_local! {
    static COMMAND_OUTCOME_CAPTURE: RefCell<Option<PluginCommandOutcome>> = const { RefCell::new(None) };
}

#[doc(hidden)]
pub fn begin_command_outcome_capture() {
    COMMAND_OUTCOME_CAPTURE.with(|slot| {
        *slot.borrow_mut() = Some(PluginCommandOutcome::default());
    });
}

#[doc(hidden)]
#[must_use]
pub fn finish_command_outcome_capture() -> PluginCommandOutcome {
    COMMAND_OUTCOME_CAPTURE
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_default()
}

/// Record metadata for the currently-running plugin command.
///
/// If no command capture is active, this is a no-op. That keeps plugin code safe
/// to call from CLI, service, and test harnesses that do not collect outcomes.
pub fn record_command_outcome_metadata(key: impl Into<String>, value: Value) {
    COMMAND_OUTCOME_CAPTURE.with(|slot| {
        if let Some(outcome) = slot.borrow_mut().as_mut() {
            outcome.metadata.insert(key.into(), value);
        }
    });
}
