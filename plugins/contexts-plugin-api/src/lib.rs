//! Typed public API of the bmux contexts plugin.
//!
//! Contexts are a higher-level grouping layer over sessions: a context
//! carries a name and an attribute map, and clients can "select" a
//! context to scope their view. Other plugins depend on this crate for
//! typed access to context queries and commands.
//!
//! The [`contexts_state`], [`contexts_commands`], and [`contexts_events`]
//! modules are generated from `bpdl/contexts-plugin.bpdl` at compile
//! time via the [`bmux_plugin_schema_macros::schema!`] macro.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/contexts-plugin.bpdl",
}

pub mod context_state;
pub use context_state::{CONTEXT_SESSION_ID_ATTRIBUTE, ContextState, RuntimeContext};

/// Capability identifiers gating access to this plugin's surfaces.
///
/// Written by hand (not BPDL-generated) until the schema language grows
/// a `[[capabilities]]` declaration.
pub mod capabilities {
    use bmux_plugin_sdk::CapabilityId;

    /// Capability gating read access to contexts-plugin query surfaces.
    pub const CONTEXTS_READ: CapabilityId = CapabilityId::from_static("bmux.contexts.read");

    /// Capability gating write access to contexts-plugin command
    /// surfaces (creating, selecting, and closing contexts).
    pub const CONTEXTS_WRITE: CapabilityId = CapabilityId::from_static("bmux.contexts.write");
}
