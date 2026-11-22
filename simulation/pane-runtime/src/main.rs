//! Runs the pane runtime simulation; see the library documentation.
//!
//! `cargo sim pane-runtime` runs it, configured like any simvar simulation through
//! `SIMULATOR_*` environment variables. `--tui` runs it continuously in the dashboard (the
//! fleet, failures, and time travel through any run); `--fleet` runs it continuously and
//! prints what it finds.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use pane_runtime::{PaneRuntimeBootstrap, telemetry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    sim_support::main(
        sim_support::Simulation {
            name: "pane-runtime",
            title: "pane runtime",
            bootstrap: PaneRuntimeBootstrap,
            views: vec![Box::new(telemetry::PanesView::default())],
            extra: telemetry::extra,
        },
        &args,
    )
}
