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
