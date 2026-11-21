//! Runs the local terminal simulation; see the library documentation.
//!
//! `cargo sim local-terminal` runs it, configured like any simvar simulation through
//! `SIMULATOR_*` environment variables.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use local_terminal::LocalTerminalBootstrap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(unknown) = std::env::args().nth(1) {
        return Err(format!("unknown option {unknown}").into());
    }
    let results = simvar::run_simulation(LocalTerminalBootstrap)?;
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
