//! Runs the pane runtime simulation; see the library documentation.
//!
//! `cargo sim pane-runtime` runs it, configured like any simvar simulation through
//! `SIMULATOR_*` environment variables. `--tui` watches the runs in a dashboard (pause, step,
//! go back and break on failures, as in the pty simulation).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use pane_runtime::{PaneRuntimeBootstrap, telemetry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags: Vec<String> = std::env::args().skip(1).collect();
    if let Some(unknown) = flags.iter().find(|flag| *flag != "--tui") {
        return Err(format!("unknown option {unknown}; expected --tui").into());
    }
    let results = if flags.is_empty() {
        simvar::run_simulation(PaneRuntimeBootstrap)?
    } else {
        sim_support::run_with_tui(
            PaneRuntimeBootstrap,
            "pane runtime simulation",
            vec![
                Box::new(telemetry::PanesPanel::default()),
                Box::new(sim_support::PtyPanel::default()),
            ],
            telemetry::extra,
        )?
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
