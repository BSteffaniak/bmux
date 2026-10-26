#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! bmux — modern terminal multiplexer written in Rust.
//!
//! Umbrella crate that re-exports the domain-agnostic bmux building
//! blocks behind optional cargo features. Domain concepts (sessions,
//! windows, panes, clients, contexts, permissions) live in plugins and
//! are consumed through their `*-plugin-api` crates — not from this
//! umbrella.
//!
//! # Features
//!
//! - `config` — configuration loader.
//! - `cli` — `bmux` binary entrypoint and runtime.
//! - `server` — server-side daemon.
//! - `client` — IPC client library.
//! - `keybind` — key-binding parser.
//! - `plugin` — host-side plugin loader and registry.
//! - `plugin_sdk` — plugin author SDK.
//! - `sandbox_harness` — integration test harness.
//! - `tui` — reusable terminal UI primitives and pointer intent.
//! - `tui_runtime` — bounded scheduling and terminal presentation.
//! - `tui_components` — opt-in reusable controls.
//! - `terminal_protocol` — terminal protocol parsing and wire types.
//!
//! These exports do not select a component set or guarantee host terminal
//! capabilities. Configure backend and component features on the underlying
//! crates; pointer intent is separate from terminal output support. These four
//! exports are opt-in and are not part of the default `all` feature group.

#[cfg(feature = "config")]
pub use bmux_config as config;

#[cfg(feature = "cli")]
pub use bmux_cli as cli;

#[cfg(feature = "server")]
pub use bmux_server as server;

#[cfg(feature = "client")]
pub use bmux_client as client;

#[cfg(feature = "keybind")]
pub use bmux_keybind as keybind;

#[cfg(feature = "plugin")]
pub use bmux_plugin as plugin;

#[cfg(feature = "plugin_sdk")]
pub use bmux_plugin_sdk as plugin_sdk;

#[cfg(feature = "sandbox_harness")]
pub use bmux_sandbox_harness as sandbox_harness;

#[cfg(feature = "tui")]
pub use bmux_tui as tui;

#[cfg(feature = "tui_runtime")]
pub use bmux_tui_runtime as tui_runtime;

#[cfg(feature = "tui_components")]
pub use bmux_tui_components as tui_components;

#[cfg(feature = "terminal_protocol")]
pub use bmux_terminal_protocol as terminal_protocol;

/// Prelude module for commonly used domain-agnostic types.
pub mod prelude {
    #[cfg(feature = "config")]
    pub use crate::config::{BmuxConfig, ConfigPaths};
}
