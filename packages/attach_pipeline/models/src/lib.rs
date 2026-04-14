#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct AttachOutputChunkMeta {
    pub stream_start: u64,
    pub stream_end: u64,
    pub stream_gap: bool,
    pub sync_update_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum AttachChunkApplyOutcome {
    Applied { had_data: bool },
    Stale,
    Desync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum AttachPipelineDiagnosticCode {
    ChunkStale,
    ChunkDesync,
    SnapshotHydratePane,
    SnapshotHydrateFull,
    ViewChangedHydrate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct AttachPipelineDiagnosticEvent {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub code: AttachPipelineDiagnosticCode,
    pub message: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub pane_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct AttachViewport {
    pub cols: u16,
    pub rows: u16,
    pub status_top_inset: u16,
    pub status_bottom_inset: u16,
}
