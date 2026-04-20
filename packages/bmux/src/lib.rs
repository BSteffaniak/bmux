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
//! - `event` — modal input event system.
//! - `config` — configuration loader.
//! - `cli` — `bmux` binary entrypoint and runtime.
//! - `server` — server-side daemon.
//! - `client` — IPC client library.
//! - `keybind` — key-binding parser.
//! - `plugin` — host-side plugin loader and registry.
//! - `plugin_sdk` — plugin author SDK.
//! - `sandbox_harness` — integration test harness.

#[cfg(feature = "event")]
pub use bmux_event as event;

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

/// Prelude module for commonly used domain-agnostic types.
pub mod prelude {
    #[cfg(feature = "event")]
    pub use crate::event::{Event, Mode};

    #[cfg(feature = "config")]
    pub use crate::config::{BmuxConfig, ConfigPaths};
}
