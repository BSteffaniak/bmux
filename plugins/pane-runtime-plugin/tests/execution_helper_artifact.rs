//! Cargo builds the real auxiliary executable for this integration target.
//! Unit fixtures use the same profile directory, never a PATH-installed helper.
#[test]
fn execution_helper_rejects_unbound_and_obsolete_invocations() {
    for args in [
        vec![],
        vec![
            "--guardian-terminal-session-v3",
            "unused",
            "unused",
            "unused",
        ],
    ] {
        let status =
            std::process::Command::new(env!("CARGO_BIN_EXE_bmux-pane-execution-supervisor"))
                .args(args)
                .status()
                .expect("execute Cargo-built helper");
        assert!(
            !status.success(),
            "unbound/obsolete helper must reject startup"
        );
    }
}
