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

use pty::{PtyBootstrap, telemetry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    sim_support::main(
        sim_support::Simulation {
            name: "pty",
            title: "bmux_pty",
            bootstrap: PtyBootstrap,
            views: Vec::new(),
            extra: telemetry::extra,
            regression_seeds: concat!(env!("CARGO_MANIFEST_DIR"), "/regression-seeds"),
        },
        &args,
    )
}
