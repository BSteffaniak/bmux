//! The pane runtime's manager speaks this crate's types in simulated runs, so it uses only what
//! a simulation controls (see `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR")).assert_clean();
}
