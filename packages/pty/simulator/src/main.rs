//! Runs the `bmux_pty` simulation; see the library documentation.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use bmux_pty_simulator::PtyBootstrap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let results = simvar::run_simulation(PtyBootstrap)?;
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
