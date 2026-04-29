//! Typed public API of the bmux snapshot plugin.
//!
//! The [`snapshot_types`], [`snapshot_state`], and
//! [`snapshot_commands`] modules are generated from
//! `bpdl/snapshot-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro. The handwritten
//! modules in this crate remain non-transport utilities: snapshot file
//! envelopes and offline mutation helpers.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod envelope;
pub mod offline_snapshot;

use std::path::PathBuf;

bmux_plugin_schema_macros::schema! {
    source: "bpdl/snapshot-plugin.bpdl",
}

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
