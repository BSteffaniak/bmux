//! Everything this crate does with threads, channels, locks and time goes through
//! `switchy_thread` and `switchy_time`, so a simulator build schedules all of it
//! deterministically (see `bmux_sim_primitives_check`). Only code marked native-only (Windows
//! and other hosts that are never simulated) may use the standard library's versions directly.

#[test]
fn threads_channels_and_time_go_through_switchy() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR"))
        // The Windows pseudo-console runner is native-only as a whole.
        .native(&["conpty.rs"])
        // This crate is a platform layer: it chooses each system's backend.
        .os_named_in(&["lib.rs"])
        // Stricter here than the shared rules: no std threads, std sync groups or std instants
        // at all.
        .forbid(&["std::thread::", "std::sync::{", "Instant::now"])
        .assert_clean();
}
