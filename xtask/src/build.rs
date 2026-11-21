//! Product build: the CLI and its installer are one unit.
use crate::artifacts::{self, Result, error};
use std::{path::Path, process::Command};

#[derive(Debug, Default)]
pub(crate) struct Features {
    pub(crate) selected: Vec<String>,
    pub(crate) all: bool,
    pub(crate) no_default: bool,
}

impl Features {
    fn append_arguments(&self, args: &mut Vec<String>) {
        for selection in &self.selected {
            args.extend(["--features".into(), selection.clone()]);
        }
        if self.all {
            args.push("--all-features".into());
        }
        if self.no_default {
            args.push("--no-default-features".into());
        }
    }
}

pub(crate) fn arguments(target: &str, profile: &str, features: &Features) -> Result<Vec<String>> {
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
    features.append_arguments(&mut args);
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
    features: &Features,
) -> Result<()> {
    let status = Command::new("cargo")
        .args(arguments(target, profile, features)?)
        .current_dir(workspace)
        .status()?;
    if !status.success() {
        return Err(error("product build failed (CLI and installer)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_build_selects_cli_and_installer_with_one_target_profile() {
        for target in ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"] {
            let args = arguments(target, "dev", &Features::default()).unwrap();
            for pair in [
                ["-p", "bmux_cli"],
                ["--bin", "bmux"],
                ["-p", "xtask"],
                ["--bin", "xtask"],
            ] {
                assert!(args.windows(2).any(|window| window == pair), "{pair:?}");
            }
            assert_eq!(args.iter().filter(|arg| *arg == "--bin").count(), 2);
            assert_eq!(args.iter().filter(|arg| *arg == "--target").count(), 1);
            assert_eq!(args.iter().filter(|arg| *arg == "--profile").count(), 1);
            assert!(args.windows(2).any(|pair| pair == ["--profile", "dev"]));
        }
        assert!(
            arguments("x86_64-unknown-linux-gnu", "release", &Features::default())
                .unwrap()
                .windows(2)
                .any(|pair| pair == ["--profile", "release"])
        );
        assert!(arguments("unknown-target", "dev", &Features::default()).is_err());
    }
}
