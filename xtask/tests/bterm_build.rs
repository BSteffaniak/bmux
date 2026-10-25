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
    let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
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
    let mut root = std::path::PathBuf::new();
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
        let reference: serde_json::Value = serde_json::from_slice(
            &fs::read(bundle.parent().unwrap().join("latest.json")).unwrap(),
        )
        .unwrap();
        root =
            std::path::PathBuf::from(reference["bundle"].as_str().unwrap()).join("Contents/MacOS");
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
            // Preserve old generation bytes; new inputs must publish a new generation.
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

#[test]
fn run_hands_exact_immutable_generations_to_bterm_without_moving_installation() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let target = "aarch64-apple-darwin";
    let artifacts = temp.path().join("artifacts");
    let source = artifacts.join(target).join("debug");
    let tools = temp.path().join("tools");
    let state = temp.path().join("state");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir(&tools).unwrap();
    fs::create_dir_all(state.join("bterm/data")).unwrap();
    fs::write(state.join("bterm/data/canonical"), b"installed data").unwrap();
    let legacy = artifacts.join("bterm-run").join(target).join("Bterm.app");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("legacy"), b"do not replace legacy bundle").unwrap();
    executable(
        &tools.join("rustc"),
        &format!("#!/bin/sh\necho 'host: {target}'\n"),
    );
    executable(
        &tools.join("cargo"),
        "#!/bin/sh\ncase \"$1\" in\nmetadata) cat \"$FIXTURE_METADATA\";;\nbuild) if [ -f \"$FIXTURE_FAIL\" ]; then exit 27; fi;;\n*) exit 99;;\nesac\n",
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
    for name in ["bmux", "xtask", "bmux-pane-execution-supervisor"] {
        executable(&source.join(name), "#!/bin/sh\nexit 97\n");
    }
    let launch_log = temp.path().join("launch.log");
    let fail = temp.path().join("fail-build");
    let path = std::env::join_paths(std::iter::once(tools.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["bterm", "run", "--workspace"])
            .arg(&workspace)
            .env("CARGO", tools.join("cargo"))
            .env("PATH", &path)
            .env("XDG_STATE_HOME", &state)
            .env("FIXTURE_METADATA", &metadata)
            .env("FIXTURE_FAIL", &fail)
            .env("FIXTURE_LAUNCH_LOG", &launch_log)
            .output()
            .unwrap()
    };
    let mut generations = Vec::new();
    let mut installation = None;
    for version in ["one", "two"] {
        // The launch fixture records the actual executable and selected root;
        // it deliberately does not simulate companion identity or readiness.
        executable(
            &source.join("bterm"),
            &format!(
                "#!/bin/sh\n# {version}\nprintf '%s\\n' \"$0\" \"$1\" \"$2\" >> \"$FIXTURE_LAUNCH_LOG\"\n"
            ),
        );
        let output = invoke();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let lines = fs::read_to_string(&launch_log).unwrap();
        let records: Vec<_> = lines.lines().collect();
        let record = &records[records.len() - 3..];
        assert_eq!(record[1], "--managed-root");
        let executable = std::path::PathBuf::from(record[0]);
        assert!(executable.is_absolute());
        assert!(executable.starts_with(artifacts.join("bterm-run").join(target).join("bundles")));
        assert!(
            fs::read_to_string(&executable)
                .unwrap()
                .contains(&format!("# {version}"))
        );
        generations.push(executable);
        let selected = std::path::PathBuf::from(record[2]);
        assert!(selected.starts_with(state.join("bterm-dev")));
        if let Some(previous) = &installation {
            assert_eq!(
                &selected, previous,
                "artifact change must not select empty durable data"
            );
            assert_eq!(
                fs::read(selected.join("data/sentinel")).unwrap(),
                b"existing development data"
            );
        } else {
            fs::create_dir_all(selected.join("data")).unwrap();
            fs::write(selected.join("data/sentinel"), b"existing development data").unwrap();
            installation = Some(selected);
        }
    }
    assert_ne!(generations[0], generations[1]);
    let unchanged = invoke();
    assert!(
        unchanged.status.success(),
        "{}",
        String::from_utf8_lossy(&unchanged.stderr)
    );
    let lines = fs::read_to_string(&launch_log).unwrap();
    let records: Vec<_> = lines.lines().collect();
    assert_eq!(std::path::PathBuf::from(records[6]), generations[1]);
    assert_eq!(std::path::PathBuf::from(records[8]), installation.unwrap());
    assert!(
        fs::read_to_string(&generations[0])
            .unwrap()
            .contains("# one")
    );
    assert_eq!(
        fs::read(legacy.join("legacy")).unwrap(),
        b"do not replace legacy bundle"
    );
    assert_eq!(
        fs::read(state.join("bterm/data/canonical")).unwrap(),
        b"installed data"
    );
    let reference = artifacts.join("bterm-run").join(target).join("latest.json");
    let before_reference = fs::read(&reference).unwrap();
    let before_log = fs::read(&launch_log).unwrap();
    fs::write(&fail, b"injected build failure").unwrap();
    let output = invoke();
    assert!(!output.status.success());
    assert_eq!(fs::read(&reference).unwrap(), before_reference);
    assert_eq!(
        fs::read(&launch_log).unwrap(),
        before_log,
        "failed build must not launch any generation"
    );
    assert!(generations.iter().all(|path| path.is_file()));
}
