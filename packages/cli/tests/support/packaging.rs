//! Shared packager helper for dylib integration tests.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Package a plugin crate that exports its own entry (`plugin!` plus the
/// `dynamic-backend` feature) as a backend dylib with the shared packager,
/// using the profile of the `bmux` binary under test. No wrapper crate is
/// generated: the packager runs `cargo rustc --crate-type cdylib` on the
/// crate itself in a target directory reserved for dylib builds.
#[must_use]
pub fn package_plugin_dylib(implementation_manifest: &Path, artifact_id: &str) -> PathBuf {
    let profile_dir = super::support::bmux_binary()
        .parent()
        .expect("bmux binary directory")
        .to_path_buf();
    let target_dir = profile_dir.parent().expect("cargo target directory");
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let mut command = Command::new("python3");
    command
        .current_dir(workspace)
        .arg("-B")
        .arg("scripts/package-plugin.py")
        .arg("--implementation-manifest")
        .arg(implementation_manifest)
        .args([
            "--component",
            "backend",
            "--build",
            "--artifact-id",
            artifact_id,
        ])
        .arg("--cargo-target-dir")
        .arg(target_dir.join("plugin-dylib"));
    if profile_dir
        .file_name()
        .is_some_and(|name| name == "release")
    {
        command.arg("--release");
    }
    let output = command
        .output()
        .expect("run shared plugin packager (python3 3.11+ required)");
    super::support::assert_success(&output, "package plugin dylib");
    // The packager prints the library Cargo reported; never guess a file name.
    let library = String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .map(PathBuf::from)
        .expect("packager prints the built library");
    assert!(library.is_file(), "missing {}", library.display());
    library
}
