//! Everything the pane runtime does with tasks, threads, locks, time, randomness, the
//! environment, processes, files and directory listings goes through what a simulation controls
//! (`switchy_async`, `switchy_thread`, `switchy_time`, `switchy_env`, `switchy_fs` and the
//! plugin's `ids`, `dir_order` and `run_shared` modules, and the platform layer's `files`), so a
//! simulator build runs it deterministically (see `bmux_sim_primitives_check` for the rules).
//!
//! Native backends (the platform layer's files that talk to one operating system and never run
//! in simulation) are exempt as a whole, as are the modules that implement the controlled
//! versions; any other line using the standard library's version directly must say why it is
//! safe with a `// simulation-safe: <reason>` comment (on it or on the line above).
//!
//! Only the platform layer (`src/platform/`) names an operating system: core logic is the same on
//! every one, so the simulation (which behaves as Linux, macOS or Windows on any host) runs the
//! paths each native build runs.

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR"))
        // Native backends: each talks to one operating system and never runs in simulation.
        .native(&["platform/unix/", "platform/windows/", "platform/native.rs"])
        // The controlled versions themselves, and code that only tests build.
        .exempt(&[
            "ids.rs",
            "dir_order.rs",
            "run_shared.rs",
            "runtime/ingest_bench.rs",
        ])
        .os_named_in(&["platform/"])
        .assert_clean();
}
