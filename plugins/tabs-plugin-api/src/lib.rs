//! Typed public API of the bmux tabs plugin.
//!
//! This crate is the stable contract other plugins depend on. The
//! [`tabs_state`], [`tabs_commands`], and [`tabs_events`]
//! modules are generated from `bpdl/tabs-plugin.bpdl` at compile time
//! via the [`bmux_plugin_schema_macros::schema!`] macro.
//!
//! Consumers pattern:
//!
//! ```ignore
//! use bmux_tabs_plugin_api::tabs_state::TabsStateService;
//!
//! fn somewhere(state: &dyn TabsStateService, id: uuid::Uuid) {
//!     let focused = state.focused_pane(id);
//!     // ...
//! }
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
// BPDL-generated command methods mirror wire records; rich commands like
// create-floating-pane naturally exceed clippy's argument-count heuristic.
#![allow(clippy::too_many_arguments)]

bmux_plugin_schema_macros::schema! {
    source: "bpdl/tabs-plugin.bpdl",
}
