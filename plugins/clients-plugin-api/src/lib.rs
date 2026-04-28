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

pub mod typed_client;
