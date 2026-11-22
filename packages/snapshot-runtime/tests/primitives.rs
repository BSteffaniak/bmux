//! The pane runtime marks snapshots dirty from simulated runs, so this crate's atomics and
//! clock are the ones a simulation controls (see `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR")).assert_clean();
}
