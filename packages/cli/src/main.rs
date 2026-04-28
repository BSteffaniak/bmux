#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let exit_code = match bmux_cli::run_cli().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("bmux error: {error:#}");
            ExitCode::from(1)
        }
    };
    bmux_plugin_sdk::perf_telemetry::flush();
    exit_code
}
