//! Exercise the public build-only entry point without building or launching a GUI.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, process::Command};

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn build_only_replaces_bundle_without_launching_or_requiring_runtime_environment() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let temp = tempfile::tempdir().unwrap();
    let target = "aarch64-apple-darwin";
    let artifacts = temp.path().join("artifacts");
    let source = artifacts.join(target).join("debug");
    fs::create_dir_all(&source).unwrap();
    let tools = temp.path().join("tools");
    fs::create_dir(&tools).unwrap();
    executable(
        &tools.join("rustc"),
        &format!("#!/bin/sh\necho 'host: {target}'\n"),
    );
    executable(
        &tools.join("cargo"),
        "#!/bin/sh\ncase \"$1\" in\nmetadata) cat \"$FIXTURE_METADATA\";;\nbuild) case \" $* \" in\n*\" --bin bmux-pane-execution-supervisor \"*) echo build >> \"$FIXTURE_BUILD_LOG\";;\n*) exit 98;;\nesac;;\n*) exit 99;;\nesac\n",
    );
    let metadata = temp.path().join("metadata.json");
    fs::write(
        &metadata,
        serde_json::to_vec(&serde_json::json!({
            "target_directory": artifacts,
        }))
        .unwrap(),
    )
    .unwrap();
    let log = temp.path().join("build.log");
    // Neither bundled binary may be executed by the build-only command.
    for name in ["bterm", "bmux", "xtask", "bmux-pane-execution-supervisor"] {
        executable(
            &source.join(name),
            "#!/bin/sh\ntouch \"$FIXTURE_LAUNCHED\"\nexit 91\n",
        );
    }
    let launched = temp.path().join("launched");
    let bundle = artifacts.join("bterm-run").join(target).join("Bterm.app");
    let root = bundle.join("Contents/MacOS");
    for iteration in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["bterm", "build", "--workspace"])
            .arg(&workspace)
            .env("CARGO", tools.join("cargo"))
            .env(
                "PATH",
                std::env::join_paths(std::iter::once(tools.clone()).chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                )))
                .unwrap(),
            )
            .env("FIXTURE_METADATA", &metadata)
            .env("FIXTURE_BUILD_LOG", &log)
            .env("FIXTURE_LAUNCHED", &launched)
            .env_remove("HOME")
            .env_remove("XDG_STATE_HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!launched.exists());
        assert!(root.join("bterm-bundle.json").is_file());
        assert!(root.join("bmux-pane-execution-supervisor").is_file());
        let inventory: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("artifacts.json")).unwrap()).unwrap();
        assert!(inventory["artifacts"].as_array().unwrap().iter().any(
            |item| item["relative_path"] == "bmux-pane-execution-supervisor"
                && item["declaration"]["id"] == "execution-supervisor"
        ));
        assert!(!root.join("obsolete").exists());
        if iteration == 0 {
            fs::write(root.join("obsolete"), "stale").unwrap();
            executable(
                &source.join("bterm"),
                "#!/bin/sh\n# rebuilt version\nexit 92\n",
            );
        }
    }
    assert_eq!(fs::read_to_string(log).unwrap(), "build\nbuild\n");
    assert!(
        fs::read_to_string(root.join("bterm"))
            .unwrap()
            .contains("rebuilt version")
    );
}
