//! Typed public API of the bmux agents plugin.
//!
//! This crate is the stable contract other plugins depend on. Everything here
//! is generated from `bpdl/agents-plugin.bpdl` by
//! [`bmux_plugin_schema_macros::schema!`]; no runtime state lives here.
//!
//! - [`agent_states`] — the retained state channel carrying every pane's
//!   agent and what it is doing.
//! - [`agents_state`] — the same snapshot on request
//!   (capability `bmux.agents.read`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/agents-plugin.bpdl",
    export: export_schema,
}
