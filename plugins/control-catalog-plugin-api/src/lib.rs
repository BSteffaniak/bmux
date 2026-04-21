//! Typed public API of the bmux control-catalog plugin.
//!
//! Aggregates session, context, and client state into a cross-cutting
//! catalog snapshot with a monotonic revision counter. Other plugins
//! and attach-side callers depend on this crate for typed catalog
//! queries and events.
//!
//! The [`control_catalog_state`] and [`control_catalog_events`] modules
//! are generated from `bpdl/control-catalog-plugin.bpdl` at compile
//! time via the [`bmux_plugin_schema_macros::schema!`] macro.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/control-catalog-plugin.bpdl",
}

pub mod typed_client;

/// Capability identifiers gating access to this plugin's surfaces.
///
/// Written by hand (not BPDL-generated) until the schema language grows
/// a `[[capabilities]]` declaration.
pub mod capabilities {
    use bmux_plugin_sdk::CapabilityId;

    /// Capability gating read access to the control-catalog plugin
    /// query surface.
    pub const CATALOG_READ: CapabilityId = CapabilityId::from_static("bmux.control_catalog.read");
}
