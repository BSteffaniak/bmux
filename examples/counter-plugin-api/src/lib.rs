//! Reference contract for the generated plugin author API.
//!
//! Everything public here is generated from `bpdl/counter.bpdl`:
//! `counter_commands::v1::CounterCommands` (provider trait),
//! `counter_commands::v1::Client` (typed client), `commands::BUMP`
//! (typed command) and `extension_points::counter_display_v1` (data,
//! behavior and wire adapter). No runtime state lives here.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/counter.bpdl",
    export: export_schema,
}
