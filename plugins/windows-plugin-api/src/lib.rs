//! Typed public API of the bmux windows plugin.
//!
//! This crate is the stable contract other plugins depend on. The
//! [`windows_state`], [`windows_commands`], and [`windows_events`]
//! modules are generated from `bpdl/windows-plugin.bpdl` at compile time
//! via the [`bmux_plugin_schema_macros::schema!`] macro.
//!
//! Consumers pattern:
//!
//! ```ignore
//! use bmux_windows_plugin_api::windows_state::WindowsStateService;
//!
//! fn somewhere(state: &dyn WindowsStateService, id: uuid::Uuid) {
//!     let focused = state.focused_pane(id);
//!     // ...
//! }
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/windows-plugin.bpdl",
}

/// Capability identifiers gating access to this plugin's surfaces.
///
/// Written by hand (not BPDL-generated) until the schema language grows
/// a `[[capabilities]]` declaration; consumers should reference these
/// constants rather than hand-typing capability strings so a rename
/// would flow through the type system.
pub mod capabilities {
    use bmux_plugin_sdk::CapabilityId;

    /// Capability gating read access to windows-plugin query surfaces
    /// (listing panes, focused-pane lookup, window enumeration, etc.).
    pub const WINDOWS_READ: CapabilityId = CapabilityId::from_static("bmux.windows.read");

    /// Capability gating write access to windows-plugin command
    /// surfaces (split, launch, focus, resize, close, zoom, restart,
    /// window lifecycle).
    pub const WINDOWS_WRITE: CapabilityId = CapabilityId::from_static("bmux.windows.write");
}
