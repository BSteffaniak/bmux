//! Typed public API of the bmux connections plugin.
//!
//! Consumers use this contract to resolve configured endpoints and invoke a
//! typed service on one selected endpoint. Endpoint selection policy remains
//! in the consuming plugin.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/connections-plugin.bpdl",
}
