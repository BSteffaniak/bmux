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

pub mod application_execution;
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
    #[cfg(feature = "bench")]
    if let Some(arguments) = benchmark_arguments(std::env::args_os()) {
        return bmux_bench_cli::run_from(arguments).map_err(anyhow::Error::msg);
    }
    #[cfg(all(unix, feature = "bundled-plugin-application-resurrection"))]
    if let Some(code) =
        bmux_application_resurrection_plugin::shell::try_run().map_err(anyhow::Error::msg)?
    {
        return Ok(code);
    }
    runtime::startup_future_marker("run_cli.first_poll", 0);
    Box::pin(runtime::run()).await
}

// Keep the optional benchmark path independent of config loading, plugin scanning,
// endpoint selection, and daemon startup. Its arguments belong to the shared frontend.
#[cfg(feature = "bench")]
fn benchmark_arguments(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Option<Vec<std::ffi::OsString>> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next()?;
    if arguments.next()?.as_os_str() != "bench" {
        return None;
    }
    Some(
        std::iter::once(std::ffi::OsString::from("bmux bench"))
            .chain(arguments)
            .collect(),
    )
}

#[cfg(all(test, feature = "bench"))]
mod benchmark_tests {
    use super::benchmark_arguments;

    #[test]
    fn delegates_only_explicit_benchmark_subcommand() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            benchmark_arguments(args(&["bmux", "bench", "run", "--profile", "quick"])),
            Some(args(&["bmux bench", "run", "--profile", "quick"]))
        );
        assert!(benchmark_arguments(args(&["bmux", "attach", "bench"])).is_none());
        assert!(benchmark_arguments(args(&["bmux", "--config", "bench"])).is_none());
        assert!(benchmark_arguments(args(&["bmux"])).is_none());
    }
}
