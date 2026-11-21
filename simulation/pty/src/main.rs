//! Runs the `bmux_pty` simulation; see the library documentation.
//!
//! `cargo sim pty` runs it, configured like any simvar simulation through `SIMULATOR_*`
//! environment variables. Its own flags:
//!
//! * `--tui`: watch the runs in a dashboard: pause and step with space, `n` and `N`, go back
//!   with `b` and `B`, and run to decisions or set breakpoints from its `:` command line.
//! * `--regression`: run every seed in `regression-seeds`, each in a fresh process and
//!   replayed to verify it is deterministic.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{path::Path, process::Command};

use pty::{PtyBootstrap, telemetry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags: Vec<String> = std::env::args().skip(1).collect();
    if let Some(unknown) = flags
        .iter()
        .find(|f| !matches!(f.as_str(), "--tui" | "--regression"))
    {
        return Err(format!("unknown option {unknown}; expected --tui or --regression").into());
    }
    if flags.iter().any(|f| f == "--regression") {
        return regression();
    }
    let results = if flags.iter().any(|f| f == "--tui") {
        with_tui()?
    } else {
        simvar::run_simulation(PtyBootstrap)?
    };
    let failed: Vec<_> = results.iter().filter(|r| !r.is_success()).collect();
    for result in &failed {
        println!("{result}");
    }
    println!("{} runs, {} failed", results.len(), failed.len());
    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// Run the simulation while a dashboard shows it, then return its results.
fn with_tui() -> Result<Vec<simvar::SimResult>, Box<dyn std::error::Error>> {
    sim_support::run_with_tui(
        PtyBootstrap,
        "bmux_pty simulation",
        vec![Box::new(telemetry::PtyPanel::default())],
        telemetry::extra,
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
