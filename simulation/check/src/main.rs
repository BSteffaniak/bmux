//! `cargo sim NAME --check`: check that everything simulation NAME links runs deterministically
//! under it, with `switchy_cargo`: every local crate it links scanned by the rules in its
//! manifest's `[package.metadata.switchy.simulation]`, and every other crate on the audit list
//! in `simulation/determinism.toml`.
//!
//! A separate program, so the check's own dependencies stay out of the simulations' graphs.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{path::Path, process::ExitCode};

fn main() -> ExitCode {
    let names: Vec<String> = std::env::args().skip(1).collect();
    if names.is_empty() {
        eprintln!("usage: sim-check NAME...; or cargo sim NAME --check");
        return ExitCode::FAILURE;
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| Path::new("Cargo.toml").to_path_buf(), |dir| dir.join("Cargo.toml"));
    let mut passed = true;
    for name in names {
        match switchy_cargo::Check::with_workspace_config(&manifest, &name)
            .and_then(|check| check.run())
        {
            Ok(report) => {
                println!("== {name}\n{report}");
                passed &= report.passed();
            }
            Err(error) => {
                eprintln!("{name}: {error}");
                passed = false;
            }
        }
    }
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
