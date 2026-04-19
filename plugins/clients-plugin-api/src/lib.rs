//! Typed public API of the bmux clients plugin.
//!
//! Tracks per-client identity, selected session, and follow state.
//! Other plugins depend on this crate for typed access to client
//! queries and commands.
//!
//! The [`clients_state`], [`clients_commands`], and [`clients_events`]
//! modules are generated from `bpdl/clients-plugin.bpdl` at compile
//! time via the [`bmux_plugin_schema_macros::schema!`] macro.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/clients-plugin.bpdl",
}

/// Capability identifiers gating access to this plugin's surfaces.
///
/// Written by hand (not BPDL-generated) until the schema language grows
/// a `[[capabilities]]` declaration.
pub mod capabilities {
    use bmux_plugin_sdk::CapabilityId;

    /// Capability gating read access to clients-plugin query surfaces.
    pub const CLIENTS_READ: CapabilityId = CapabilityId::from_static("bmux.clients.read");

    /// Capability gating write access to clients-plugin command
    /// surfaces.
    pub const CLIENTS_WRITE: CapabilityId = CapabilityId::from_static("bmux.clients.write");
}
