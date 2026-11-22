//! The pane runtime keeps each pane's grid with this crate in simulated runs, so it uses only
//! what a simulation controls (see `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR")).assert_clean();
}
