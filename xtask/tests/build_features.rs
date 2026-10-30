//! Verify feature forwarding at the public task boundary without a product build.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt as _, process::Command};

#[test]
fn product_build_forwards_cargo_feature_selection() {
    let root = tempfile::tempdir().unwrap();
    let tools = root.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let cargo = tools.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BUILD_ARGS\"\n",
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let log = root.path().join("args");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for (selection, expected) in [
        (vec![], vec![]),
        (
            vec![
                "--features",
                "bmux_cli/bench,bmux_cli/compression",
                "--features=bmux_cli/kitty-keyboard bmux_cli/bracketed-paste",
            ],
            vec![
                "--features",
                "bmux_cli/bench,bmux_cli/compression",
                "--features",
                "bmux_cli/kitty-keyboard bmux_cli/bracketed-paste",
            ],
        ),
        (
            vec![
                "--no-default-features",
                "--all-features",
                "--features",
                "bmux_cli/bench",
            ],
            vec![
                "--features",
                "bmux_cli/bench",
                "--all-features",
                "--no-default-features",
            ],
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args([
                "build",
                "--target",
                "aarch64-apple-darwin",
                "--profile",
                "release",
                "--workspace",
            ])
            .arg(&workspace)
            .args(selection)
            .env("PATH", &path)
            .env("BUILD_ARGS", &log)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = fs::read_to_string(&log).unwrap();
        let args: Vec<_> = text.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["--profile", "release"]));
        assert!(args.windows(2).any(|pair| pair == ["--bin", "bmux"]));
        assert!(args.windows(2).any(|pair| pair == ["--bin", "xtask"]));
        let feature_start = args
            .iter()
            .position(|arg| {
                matches!(
                    *arg,
                    "--features" | "--all-features" | "--no-default-features"
                )
            })
            .unwrap_or(args.len());
        assert_eq!(&args[feature_start..], expected);
    }
}

#[test]
fn invalid_feature_options_fail_before_building() {
    for args in [
        vec!["build", "--features"],
        vec!["build", "--features="],
        vec!["build", "--features", "  "],
        vec!["build", "--features", "--all-features"],
        vec!["build", "--all-features=true"],
        vec!["build", "--no-default-features=false"],
        vec!["auxiliary", "build", "--features", "bmux_cli/bench"],
        vec!["bterm", "build", "--all-features"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("xtask:"));
    }
}
