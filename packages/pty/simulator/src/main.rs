//! Runs the `bmux_pty` simulation; see the library documentation.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::sync::Arc;

use bmux_pty_simulator::{PtyBootstrap, telemetry};
use simvar::observer::ReplayControl;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let results = if std::env::args().any(|arg| arg == "--tui") {
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
fn with_tui() -> Result<Vec<simvar::SimResult>, Box<dyn std::error::Error>> {
    let replay = Arc::new(ReplayControl::default());
    telemetry::enable(Arc::clone(&replay));
    let (observer, events) = bmux_simvar_tui::observer();
    simvar::add_observer(observer);
    simvar::set_quiet(true);
    let simulation = std::thread::spawn(|| {
        simvar::run_simulation(PtyBootstrap).map_err(|error| error.to_string())
    });
    bmux_simvar_tui::Dashboard::new(
        "bmux_pty simulation",
        events,
        vec![Box::new(telemetry::PtyPanel::default())],
        Some(replay),
    )
    .run()?;
    // The user left: start no more runs, and let the current ones finish.
    simvar::end_sim();
    Ok(simulation
        .join()
        .map_err(|_| "the simulation thread panicked")??)
}
