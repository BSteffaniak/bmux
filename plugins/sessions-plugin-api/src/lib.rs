//! Typed public API of the bmux sessions plugin.
//!
//! This crate is the stable contract other plugins depend on for the
//! session domain. The [`sessions_state`], [`sessions_commands`], and
//! [`sessions_events`] modules are generated from
//! `bpdl/sessions-plugin.bpdl` at compile time via the
//! [`bmux_plugin_schema_macros::schema!`] macro.
//!
//! Consumers pattern:
//!
//! ```ignore
//! use bmux_sessions_plugin_api::sessions_state::SessionsStateService;
//!
//! fn somewhere(state: &dyn SessionsStateService) {
//!     let sessions = state.list_sessions();
//!     // ...
//! }
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/sessions-plugin.bpdl",
}

pub mod session_manager;
pub use session_manager::SessionManager;

/// Capability identifiers gating access to this plugin's surfaces.
///
/// Written by hand (not BPDL-generated) until the schema language grows
/// a `[[capabilities]]` declaration; consumers should reference these
/// constants rather than hand-typing capability strings so a rename
/// would flow through the type system.
pub mod capabilities {
    use bmux_plugin_sdk::CapabilityId;

    /// Capability gating read access to sessions-plugin query surfaces
    /// (listing sessions, selector lookups).
    pub const SESSIONS_READ: CapabilityId = CapabilityId::from_static("bmux.sessions.read");

    /// Capability gating write access to sessions-plugin command
    /// surfaces (creating, killing, and selecting sessions).
    pub const SESSIONS_WRITE: CapabilityId = CapabilityId::from_static("bmux.sessions.write");
}
