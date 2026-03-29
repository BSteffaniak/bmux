#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

mod connection;
mod input;
mod playbook;
mod runtime;
mod status;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match runtime::run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("bmux error: {error:#}");
            ExitCode::from(1)
        }
    }
}
