//! The pane runtime's sessions are these models in simulated runs, so they use only what a
//! simulation controls (see `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR")).assert_clean();
}
