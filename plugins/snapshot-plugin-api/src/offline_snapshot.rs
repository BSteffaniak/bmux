//! Offline snapshot-mutation utilities.
//!
//! When the server is not running, CLI subcommands like
//! `bmux kill-session` still need to be able to prune entries from
//! the persisted snapshot so they take effect the next time the
//! server starts. This module will host those utilities (relocated
//! from `bmux_server::offline_kill_sessions`).
//!
//! # Status
//!
//! Slice 13 **Stage 4** landed this module as a placeholder. The
//! actual `offline_kill_sessions` implementation relocates here in
//! **Stage 6**, after the monolithic `SnapshotV4` schema has been
//! deleted from the server (Stage 5) and the combined-envelope format
//! owned by the snapshot plugin is the canonical format.

// Placeholder: utilities land here in Stage 6.
