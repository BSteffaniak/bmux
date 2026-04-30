//! Neutral recording protocol DTOs.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingProfile {
    Full,
    Functional,
    Visual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingEventKind {
    PaneInputRaw,
    PaneOutputRaw,
    ProtocolReplyRaw,
    PaneImage,
    ServerEvent,
    RequestStart,
    RequestDone,
    RequestError,
    Custom,
}
