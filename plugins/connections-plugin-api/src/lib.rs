//! Typed public API of the bmux connections plugin.
//!
//! Remote hosting administration and status, and typed service invocation
//! on a selected endpoint (`local` or a saved host).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/connections-plugin.bpdl",
    export: export_schema,
}
