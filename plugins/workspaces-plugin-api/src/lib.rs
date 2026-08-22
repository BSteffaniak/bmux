//! Stable typed contract for the bmux workspaces plugin.
//!
//! Workspace state, commands, events, and list snapshots are generated from
//! the BPDL schema. Runtime state remains in `bmux_workspaces_plugin`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/workspaces-plugin.bpdl",
}
