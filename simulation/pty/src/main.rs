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

use std::{path::Path, process::Command, sync::Arc};

use bmux_simvar_tui::Exit;
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

/// Run the simulation on another thread while a dashboard shows it, then return its results.
/// Going back in a run replaces this process with a rerun of its seed.
fn with_tui() -> Result<Vec<simvar::SimResult>, Box<dyn std::error::Error>> {
    let replay = bmux_simvar_tui::replay_control();
    telemetry::enable(Arc::clone(&replay));
    let (observer, events) = bmux_simvar_tui::observer();
    simvar::add_observer(observer);
    simvar::set_quiet(true);
    let simulation = std::thread::spawn(|| {
        simvar::run_simulation(PtyBootstrap).map_err(|error| error.to_string())
    });
    let exit = bmux_simvar_tui::Dashboard::new(
        "bmux_pty simulation",
        events,
        vec![Box::new(telemetry::PtyPanel::default())],
        Some(replay),
    )
    .run()?;
    if let Exit::Seek { seed, decision } = exit {
        return Err(bmux_simvar_tui::seek(seed, decision).into());
    }
    // The user left: start no more runs, and let the current ones finish.
    simvar::end_sim();
    Ok(simulation
        .join()
        .map_err(|_| "the simulation thread panicked")??)
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
