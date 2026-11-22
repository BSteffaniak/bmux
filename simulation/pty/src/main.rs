//! Runs the `bmux_pty` simulation; see the library documentation.
//!
//! `cargo sim pty` runs it, configured like any simvar simulation through `SIMULATOR_*`
//! environment variables. Its own flags:
//!
//! * `--tui [SEED[:EPOCH]]`: run continuously in the dashboard (the fleet, failures, and time
//!   travel through any run), with the run of `SEED` open when given.
//! * `--fleet`: run continuously without the dashboard, printing what it finds.
//! * `--regression`: run every seed in `regression-seeds`, each in a fresh process and
//!   replayed to verify it is deterministic.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{path::Path, process::Command};

use pty::{PtyBootstrap, telemetry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--regression") {
        return regression();
    }
    sim_support::main(
        sim_support::Simulation {
            name: "pty",
            title: "bmux_pty",
            bootstrap: PtyBootstrap,
            views: Vec::new(),
            extra: telemetry::extra,
        },
        &args,
    )
}

/// Run every seed in `regression-seeds` (the first word of each line that is not blank or a
/// comment), each alone in a fresh process with determinism verification.
fn regression() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("regression-seeds");
    let seeds = std::fs::read_to_string(&path)?;
    let executable = std::env::current_exe()?;
    let mut failed = Vec::new();
    for seed in seeds
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| !word.starts_with('#'))
    {
        let status = Command::new(&executable)
            .env("SIMULATOR_SEED", seed)
            .env("SIMULATOR_RUNS", "1")
            .env("SIMULATOR_MAX_PARALLEL", "1")
            .env("SIMULATOR_VERIFY_DETERMINISM", "1")
            .stdout(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            eprintln!(
                "regression seed {seed} failed; replay with: SIMULATOR_SEED={seed} cargo sim pty"
            );
            failed.push(seed.to_owned());
        }
    }
    if failed.is_empty() {
        println!("all regression seeds passed");
        Ok(())
    } else {
        Err(format!("{} regression seeds failed", failed.len()).into())
    }
}
