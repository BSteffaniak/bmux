//! Runs the pane runtime simulation; see the library documentation.
//!
//! `cargo sim pane-runtime` runs it, configured like any simvar simulation through
//! `SIMULATOR_*` environment variables. `--tui` runs it continuously in the dashboard (the
//! fleet, failures, and time travel through any run); `--fleet` runs it continuously and
//! prints what it finds; `--regression` runs every seed in `regression-seeds`. `--guided`
//! (with either of the first two) steers runs toward shapes not seen yet; see
//! `pane_runtime::guide`. `--explore <scenario|all> [--preemptions N]` runs short scripted
//! scenarios under every schedule within a preemption bound; see `pane_runtime::explore`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use pane_runtime::{PaneRuntimeBootstrap, explore, guide, telemetry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--explore") {
        return explore::main(&args[1..]);
    }
    // Guided runs steer toward shapes not seen yet (see `pane_runtime::guide`).
    if let Some(at) = args.iter().position(|arg| arg == "--guided") {
        args.remove(at);
        guide::enable();
    }
    if std::env::var_os("PANE_RUNTIME_SIM_GUIDED").is_some() {
        guide::enable();
    }
    // Count outcomes without guiding, to compare with `--guided`.
    if std::env::var_os("PANE_RUNTIME_SIM_OUTCOMES").is_some() {
        guide::track();
    }
    let result = sim_support::main(
        sim_support::Simulation {
            name: "pane-runtime",
            title: "pane runtime",
            bootstrap: PaneRuntimeBootstrap,
            views: vec![Box::new(telemetry::PanesView::default())],
            extra: telemetry::extra,
            regression_seeds: concat!(env!("CARGO_MANIFEST_DIR"), "/regression-seeds"),
        },
        &args,
    );
    if guide::tracking() {
        let (corpus, outcomes) = guide::progress();
        let mode = if guide::enabled() { "guided" } else { "unguided" };
        eprintln!("{mode}: {outcomes} distinct outcomes ({corpus} profiles kept)");
    }
    result
}
