//! Typed public API of the bmux snapshot plugin.
//!
//! Hand-written (no BPDL). Hosts the shared config type,
//! capability/interface ids, typed request/response wire enums, the
//! `typed_client` helper module, and a placeholder `offline_snapshot`
//! module for the offline snapshot-mutation utility that relocates
//! here in Slice 13 Stage 6.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod envelope;
pub mod offline_snapshot;
pub mod typed_client;

use std::path::PathBuf;

use bmux_plugin_sdk::{CapabilityId, InterfaceId};
use serde::{Deserialize, Serialize};

// ── Capabilities / interfaces ───────────────────────────────────────

/// Capability gating read access to the snapshot plugin (status / dry-run).
pub const SNAPSHOT_READ: CapabilityId = CapabilityId::from_static("bmux.snapshot.read");

/// Capability gating write access to the snapshot plugin (save / restore-apply).
pub const SNAPSHOT_WRITE: CapabilityId = CapabilityId::from_static("bmux.snapshot.write");

/// Interface id for the snapshot plugin's typed command surface.
pub const SNAPSHOT_COMMANDS_INTERFACE: InterfaceId = InterfaceId::from_static("snapshot-commands");

// ── Plugin config (CLI-registered) ──────────────────────────────────

/// Configuration consumed by the snapshot plugin at activation.
///
/// The CLI registers this into the plugin state registry BEFORE
/// `activate_loaded_plugins` runs so the plugin can read it during
/// `activate`. Missing config means snapshot persistence is disabled
/// (the plugin activates with a no-op orchestrator).
#[derive(Debug, Clone)]
pub struct SnapshotPluginConfig {
    /// On-disk path for the combined snapshot file.
    pub snapshot_path: PathBuf,
    /// Debounce window in milliseconds between dirty-mark and flush.
    /// A zero value means "flush on next tick"; typical values are
    /// in the 500–5000 range.
    pub debounce_ms: u64,
}

impl SnapshotPluginConfig {
    /// Convenience constructor.
    #[must_use]
    pub const fn new(snapshot_path: PathBuf, debounce_ms: u64) -> Self {
        Self {
            snapshot_path,
            debounce_ms,
        }
    }
}

// ── Wire request / response ────────────────────────────────────────

/// Typed request variants for the snapshot plugin's
/// `snapshot-commands::dispatch(SnapshotRequest) -> SnapshotResponse`
/// service surface.
///
/// Uses external tagging (`rename_all = "snake_case"`, no `tag = "..."`)
/// because `bmux_codec` does not support `deserialize_any` which
/// internal tagging requires. Matches BPDL codegen conventions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotRequest {
    /// Force an immediate save, bypassing the debounce window.
    SaveNow,
    /// Return current status (path, whether a snapshot file exists,
    /// last-write/restore timestamps).
    Status,
    /// Decode the on-disk snapshot without applying it; useful for
    /// `bmux server restore --dry-run`.
    RestoreDryRun,
    /// Clear every participant's state and apply the on-disk
    /// snapshot as a full replacement.
    RestoreApply,
}

/// Typed response variants. External tagging — same rationale as
/// [`SnapshotRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotResponse {
    /// `SaveNow` completed. `path` is the written file path, or
    /// `None` when persistence is disabled.
    Saved { path: Option<String> },
    /// `Status` result.
    Status(SnapshotStatusPayload),
    /// `RestoreDryRun` result.
    DryRun { ok: bool, message: String },
    /// `RestoreApply` aggregate counts.
    Applied {
        restored_plugins: u64,
        failed_plugins: u64,
    },
    /// Generic per-op error. Carries a short `code` and a human-readable
    /// `message`.
    Error { code: String, message: String },
}

/// Wire payload for `SnapshotResponse::Status`. Mirrors the shape of
/// `bmux_snapshot_runtime::SnapshotStatusReport` at the wire level.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotStatusPayload {
    pub enabled: bool,
    #[serde(default)]
    pub path: Option<String>,
    pub snapshot_exists: bool,
    #[serde(default)]
    pub last_write_epoch_ms: Option<u64>,
    #[serde(default)]
    pub last_restore_epoch_ms: Option<u64>,
    #[serde(default)]
    pub last_restore_error: Option<String>,
}
