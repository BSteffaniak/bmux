#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::process::ExitCode;

fn main() -> ExitCode {
    // Match #[tokio::main]'s default multi-thread runtime configuration, but own
    // its actual lifetime explicitly rather than just retaining borrowed Handles.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bmux runtime error: {error}");
            return ExitCode::from(1);
        }
    };
    let driver = match bmux_cli::runtime_owner::RuntimeDriver::new(runtime) {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("bmux runtime owner error: {error}");
            return ExitCode::from(1);
        }
    };
    let owner = driver.owner();
    let result = owner.handle().block_on(async {
        if std::env::var_os("BMUX_DIAG_STARTUP_FUTURES").is_some() {
            eprintln!("startup.future main.first_poll");
        }
        bmux_cli::run_cli_with_runtime(owner.clone()).await
    });
    drop(owner);
    let exit_code = match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("bmux error: {error:#}");
            ExitCode::from(1)
        }
    };
    bmux_plugin_sdk::perf_telemetry::flush();
    if driver.shutdown().is_err() {
        eprintln!("bmux primary runtime shutdown failed");
        return ExitCode::from(1);
    }
    exit_code
}
