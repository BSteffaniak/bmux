//! Runs the local terminal simulation; see the library documentation.
//!
//! `cargo sim local-terminal` runs it, configured like any simvar simulation through
//! `SIMULATOR_*` environment variables. `--tui` runs it continuously in the dashboard;
//! `--fleet` runs it continuously and prints what it finds; `--regression` runs every seed in
//! `regression-seeds`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use local_terminal::LocalTerminalBootstrap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    sim_support::main(
        sim_support::Simulation {
            name: "local-terminal",
            title: "local terminal",
            bootstrap: LocalTerminalBootstrap,
            views: Vec::new(),
            extra: Vec::new,
            regression_seeds: concat!(env!("CARGO_MANIFEST_DIR"), "/regression-seeds"),
        },
        &args,
    )
}
