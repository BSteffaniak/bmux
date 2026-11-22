//! The pane runtime reaches the session manager through this crate's handle (its no-op when no
//! sessions plugin is loaded, as in simulated runs), so it uses only what a simulation controls
//! (see `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR")).assert_clean();
}
