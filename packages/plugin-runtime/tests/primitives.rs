//! A simulated pane runtime reports shutdown milestones through this crate's shutdown progress
//! (and runs nothing else of the plugin runtime: no scopes, background tasks, guest ABI or
//! shutdown coordinator), so that uses only what a simulation controls (see
//! `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR"))
        .only(&["shutdown_progress.rs"])
        .assert_clean();
}
