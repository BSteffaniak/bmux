#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for bmux terminal multiplexer
//!
//! This package provides the command-line interface functionality for bmux.

pub use bmux_config::{BmuxConfig, ConfigPaths};
pub use bmux_event::{Event, EventDispatcher, ModalSystem, Mode};

mod connection;
mod runtime;
pub(crate) mod sandbox_meta;
mod ssh_access;
mod status;

pub mod attach;
pub mod input;

/// Playbook system for headless scripted bmux execution.
pub mod playbook;

/// Run the bmux CLI runtime entrypoint.
///
/// # Errors
/// Returns an error when CLI parsing, command execution, or runtime startup fails.
pub async fn run_cli() -> anyhow::Result<u8> {
    Box::pin(runtime::run()).await
}
