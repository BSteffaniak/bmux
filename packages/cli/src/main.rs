#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

mod cli;
mod input;
mod pane;
mod pty;
mod runtime;
mod status;
mod terminal;

use std::process::ExitCode;

fn main() -> ExitCode {
    match runtime::run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("bmux error: {error}");
            ExitCode::from(1)
        }
    }
}
