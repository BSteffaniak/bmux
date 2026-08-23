#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for bmux terminal multiplexer
//!
//! This package provides the command-line interface functionality for bmux.

mod connection;
mod pane_runtime_client;
mod runtime;
pub(crate) mod sandbox_meta;
mod ssh_access;

pub mod attach;
pub mod input;

/// Playbook system for headless scripted bmux execution.
pub mod playbook;

pub(crate) fn reqwest_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

/// Run the bmux CLI runtime entrypoint.
///
/// # Errors
/// Returns an error when CLI parsing, command execution, or runtime startup fails.
pub async fn run_cli() -> anyhow::Result<u8> {
    Box::pin(runtime::run()).await
}
