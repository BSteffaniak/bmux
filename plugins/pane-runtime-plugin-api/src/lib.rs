//! Typed public API of the bmux pane-runtime plugin.
//!
//! This crate is the stable contract core + other plugins depend on
//! for the pane-runtime domain. Five modules are generated from
//! `bpdl/pane-runtime-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro:
//! - [`pane_runtime_state`] — queries over pane/session runtime.
//! - [`pane_runtime_commands`] — mutating pane + session-runtime commands.
//! - [`attach_runtime_commands`] — per-client attach lifecycle.
//! - [`attach_runtime_state`] — attach-view queries.
//! - [`pane_runtime_events`] — lifecycle event stream.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// BPDL-generated service methods accept one argument per record
// field, which trips `too_many_arguments` on rich commands like
// `launch-pane` (8 args: session_id, target, direction, ratio_percent,
// name, program, args, cwd). The macro-generated code cannot be
// refactored; allow at the crate level.
#![allow(clippy::too_many_arguments)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/pane-runtime-plugin.bpdl",
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneRuntimePluginConfig {
    pub shell: String,
    pub pane_term: String,
    #[serde(default)]
    pub shell_integration_root: Option<std::path::PathBuf>,
}

/// Typed-client helpers for invoking this plugin's services from any
/// `TypedDispatchClient` (production `bmux_client`, tests, mocks).
pub mod typed_client;
