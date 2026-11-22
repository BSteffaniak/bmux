//! The pane runtime answers each pane's terminal queries with this crate in simulated runs, so
//! it uses only what a simulation controls (see `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR"))
        // Pointer-shape output to the user's own terminal: client side, never in a pane runtime.
        .exempt(&["pointer_output.rs"])
        .assert_clean();
}
