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

/// Capability identifiers gating access to this plugin's surfaces.
pub mod capabilities {
    use bmux_plugin_sdk::CapabilityId;

    /// Capability gating read access to pane-runtime query surfaces.
    pub const PANE_RUNTIME_READ: CapabilityId = CapabilityId::from_static("bmux.pane_runtime.read");

    /// Capability gating write access to pane-runtime command surfaces.
    pub const PANE_RUNTIME_WRITE: CapabilityId =
        CapabilityId::from_static("bmux.pane_runtime.write");

    /// Capability gating attach-lifecycle commands. Separate from
    /// pane-runtime.write because attach is a per-client concern and
    /// may be gated differently by the permissions plugin.
    pub const ATTACH_RUNTIME_WRITE: CapabilityId =
        CapabilityId::from_static("bmux.attach_runtime.write");

    /// Capability gating attach-state queries.
    pub const ATTACH_RUNTIME_READ: CapabilityId =
        CapabilityId::from_static("bmux.attach_runtime.read");
}
