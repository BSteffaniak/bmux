//! Runs the pane runtime simulation; see the library documentation.
//!
//! `cargo sim pane-runtime` runs it, configured like any simvar simulation through
//! `SIMULATOR_*` environment variables.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use pane_runtime::PaneRuntimeBootstrap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags: Vec<String> = std::env::args().skip(1).collect();
    if let Some(unknown) = flags.first() {
        return Err(format!("unknown option {unknown}").into());
    }
    let results = simvar::run_simulation(PaneRuntimeBootstrap)?;
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
