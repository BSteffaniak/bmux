//! Manifest-driven product build: the CLI, installer and declared helpers are one unit.
use crate::artifacts::{self, Result, error};
use serde_json::Value;
use std::{path::Path, process::Command};

pub(crate) fn arguments(
    target: &str,
    profile: &str,
    declarations: &[Value],
) -> Result<Vec<String>> {
    artifacts::unix_target(target)?;
    let mut args: Vec<String> = [
        "build",
        "--locked",
        "--target",
        target,
        "--profile",
        profile,
        "-p",
        "bmux_cli",
        "--bin",
        "bmux",
        "-p",
        "xtask",
        "--bin",
        "xtask",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for item in declarations {
        if artifacts::supports_target(item, target)? {
            args.extend([
                "-p".into(),
                artifacts::field(item, "cargo_package")?.into(),
                "--bin".into(),
                artifacts::field(item, "cargo_binary")?.into(),
            ]);
        }
    }
    Ok(args)
}

pub(crate) fn host_target(workspace: &Path) -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .current_dir(workspace)
        .output()?;
    if !output.status.success() {
        return Err(error("cannot determine compiler host target"));
    }
    String::from_utf8(output.stdout)?
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .ok_or_else(|| error("compiler did not report a host target"))
}

pub(crate) fn run(
    workspace: &Path,
    target: &str,
    profile: &str,
    declarations: &[Value],
) -> Result<()> {
    let status = Command::new("cargo")
        .args(arguments(target, profile, declarations)?)
        .current_dir(workspace)
        .status()?;
    if !status.success() {
        return Err(error(
            "product build failed (CLI, installer and declared helpers)",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_build_selects_cli_installer_and_every_helper_with_one_target_profile() {
        let declarations =
            vec![serde_json::json!({"cargo_package": "worker_plugin", "cargo_binary": "worker"})];
        let args = arguments("x86_64-unknown-linux-gnu", "dev", &declarations).unwrap();
        for name in ["bmux_cli", "bmux", "xtask", "worker_plugin", "worker"] {
            assert!(args.iter().any(|arg| arg == name));
        }
        assert_eq!(args.iter().filter(|arg| *arg == "--target").count(), 1);
        assert_eq!(args.iter().filter(|arg| *arg == "--profile").count(), 1);
        assert!(args.windows(2).any(|pair| pair == ["--profile", "dev"]));
        assert!(
            arguments("x86_64-unknown-linux-gnu", "release", &declarations)
                .unwrap()
                .windows(2)
                .any(|pair| pair == ["--profile", "release"])
        );
    }

    #[test]
    fn windows_build_includes_opted_in_helpers_but_not_legacy_unix_helpers() {
        let declarations = vec![
            serde_json::json!({"cargo_package": "fixture_helper", "cargo_binary": "fixture-helper", "supported_platforms": ["unix", "windows"]}),
            serde_json::json!({"cargo_package": "unix_plugin", "cargo_binary": "unix-helper"}),
        ];
        let args = arguments("x86_64-pc-windows-msvc", "release", &declarations).unwrap();
        assert!(args.iter().any(|arg| arg == "fixture-helper"));
        assert!(args.iter().any(|arg| arg == "fixture_helper"));
        assert!(!args.iter().any(|arg| arg == "unix-helper"));
        let unix = arguments("x86_64-apple-darwin", "release", &declarations).unwrap();
        assert!(unix.iter().any(|arg| arg == "unix-helper"));
        assert!(unix.iter().any(|arg| arg == "fixture-helper"));
    }

    #[test]
    fn unsupported_helpers_are_not_built_on_windows() {
        let declarations =
            vec![serde_json::json!({"cargo_package": "worker_plugin", "cargo_binary": "worker"})];
        let args = arguments("x86_64-pc-windows-msvc", "release", &declarations).unwrap();
        assert!(!args.iter().any(|arg| arg == "worker"));
        assert!(args.iter().any(|arg| arg == "bmux"));
    }
}
