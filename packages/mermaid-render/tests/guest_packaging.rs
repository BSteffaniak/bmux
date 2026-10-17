//! Staging must work with only the inputs shipped in a registry archive.

#[path = "../guest_build.rs"]
mod guest_build;

#[test]
fn stages_an_independent_locked_workspace_outside_the_package() {
    let inputs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("guest");
    let temporary = tempfile::tempdir().unwrap();
    let guest = temporary.path().join("guest");
    let font = b"fixture font";
    guest_build::prepare(&inputs, &guest, font).unwrap();
    assert_eq!(
        std::fs::read(guest.join("BmuxDiagram-Regular.ttf")).unwrap(),
        font
    );
    let lock = std::fs::read(guest.join("Cargo.lock")).unwrap();
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(&guest)
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--locked",
            "--format-version",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(guest.join("Cargo.lock")).unwrap(), lock);
    // Restaging must refresh inputs without touching the package source tree.
    guest_build::prepare(&inputs, &guest, b"replacement font").unwrap();
    assert_eq!(
        std::fs::read(guest.join("BmuxDiagram-Regular.ttf")).unwrap(),
        b"replacement font"
    );
}
