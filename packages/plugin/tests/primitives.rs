//! A simulated pane runtime keeps its host state in this crate's state registry (and nothing
//! else of the plugin host: no loader, discovery, event bus or executable identity runs in a
//! simulation), so the registry uses only what a simulation controls (see
//! `bmux_sim_primitives_check`).

#[test]
fn simulated_primitives_only() {
    bmux_sim_primitives_check::Scan::new(env!("CARGO_MANIFEST_DIR"))
        .only(&["plugin_state.rs"])
        .assert_clean();
}
