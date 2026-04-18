//! Typed public API of the bmux windows plugin.
//!
//! This crate is the stable contract other plugins depend on. The
//! [`windows_state`], [`windows_commands`], and [`windows_events`]
//! modules are generated from `bpdl/windows-plugin.bpdl` at compile time
//! via the [`bmux_plugin_schema_macros::schema!`] macro.
//!
//! Consumers pattern:
//!
//! ```ignore
//! use bmux_windows_plugin_api::windows_state::WindowsState;
//!
//! fn somewhere(state: &dyn WindowsState, id: uuid::Uuid) {
//!     let focused = state.focused_pane(id);
//!     // ...
//! }
//! ```

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

bmux_plugin_schema_macros::schema!("bpdl/windows-plugin.bpdl");
