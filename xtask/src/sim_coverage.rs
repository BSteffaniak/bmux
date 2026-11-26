//! `cargo sim NAME --coverage`: check the `runs = false` claims of what simulation NAME links
//! against what its runs execute.
//!
//! A crate marked `runs = false` in its `[package.metadata.switchy.simulation]` is linked but
//! never run inside a run, so `cargo sim NAME --check` does not scan it. This checks the claim:
//! it builds NAME instrumented for coverage (in `target/sim-coverage`), runs it (configured by
//! the `SIMULATOR_*` environment, as `cargo sim NAME` is), merges the profiles with LLVM's
//! tools, and has `sim-check` fail if any file of such a crate ran.
//!
//! LLVM's tools come from `LLVM_PROFDATA` and `LLVM_COV`, else the toolchain's `llvm-tools`
//! component (`rustup component add llvm-tools-preview`, or `nix build .#llvm-tools`), else
//! `PATH`.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::artifacts::{Result, error};

/// Run the check for simulation `name` in the simulation workspace at `simulation`.
pub fn check(cargo: &std::ffi::OsStr, simulation: &Path, name: &str, args: &[String]) -> Result<()> {
    let target = simulation
        .parent()
        .unwrap_or(simulation)
        .join("target")
        .join("sim-coverage");
    let profiles = target.join("profiles");
    if profiles.exists() {
        std::fs::remove_dir_all(&profiles)?;
    }
    std::fs::create_dir_all(&profiles)?;

    let status = Command::new(cargo)
        .current_dir(simulation)
        .env("CARGO_TARGET_DIR", &target)
        // Instrumented build scripts write profiles too: here, not in the packages' directories.
        .env("LLVM_PROFILE_FILE", target.join("build-profiles").join("%p-%m.profraw"))
        // Joined with simulation/.cargo/config.toml's flags.
        .args(["build", "--release", "--config", r#"build.rustflags=["-C", "instrument-coverage"]"#])
        .args(["--package", name])
        .status()?;
    if !status.success() {
        return Err(error(format!("building {name} for coverage failed")));
    }
    let binary = target.join("release").join(name);

    let ran = Command::new(&binary)
        .current_dir(simulation)
        .args(args)
        .env("LLVM_PROFILE_FILE", profiles.join("%p-%m.profraw"))
        .status()?;
    if !ran.success() {
        eprintln!("xtask: {name} failed; its coverage is still checked");
    }

    let merged = target.join("merged.profdata");
    let mut merge = Command::new(tool("LLVM_PROFDATA", "llvm-profdata")?);
    merge.args(["merge", "--sparse", "-o"]).arg(&merged);
    for profile in std::fs::read_dir(&profiles)? {
        merge.arg(profile?.path());
    }
    if !merge.status()?.success() {
        return Err(error("llvm-profdata merge failed"));
    }
    let export = Command::new(tool("LLVM_COV", "llvm-cov")?)
        .args(["export", "-summary-only", "-instr-profile"])
        .arg(&merged)
        .arg(&binary)
        .output()?;
    if !export.status.success() {
        return Err(error(format!(
            "llvm-cov export failed: {}",
            String::from_utf8_lossy(&export.stderr)
        )));
    }
    let summary = target.join("coverage.json");
    std::fs::write(&summary, export.stdout)?;

    let status = Command::new(cargo)
        .current_dir(simulation)
        .args(["run", "--release", "--package", "sim-check", "--", name, "--coverage"])
        .arg(&summary)
        .status()?;
    if status.success() && ran.success() {
        Ok(())
    } else {
        std::process::exit(status.code().filter(|_| !status.success()).unwrap_or(1));
    }
}

/// LLVM tool `name`: from `variable`, the toolchain's `llvm-tools`, or `PATH`.
fn tool(variable: &str, name: &str) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(variable) {
        return Ok(path.into());
    }
    let sysroot = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .args(["--print", "sysroot"])
        .output()?;
    let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim());
    if let Ok(targets) = std::fs::read_dir(sysroot.join("lib").join("rustlib")) {
        for target in targets.flatten() {
            let candidate = target.path().join("bin").join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let found = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    });
    found.ok_or_else(|| {
        error(format!(
            "{name} not found: set {variable}, or install the toolchain's llvm-tools \
             (rustup component add llvm-tools-preview, or nix build .#llvm-tools)"
        ))
    })
}
