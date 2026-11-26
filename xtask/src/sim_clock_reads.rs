//! `cargo sim NAME --clock-reads [ARGS]`: run simulation NAME with `switchy_clock_probe` loaded
//! into its process, listing the real clock reads its runs make (simvar's
//! `SIMULATOR_CLOCK_READS`: `report` unless set, or `fail`).
//!
//! The probe comes from the MoosicBox checkout the simulation workspace builds simvar from
//! (found with `cargo metadata`). It and NAME are built in `target/sim-clock-reads`, NAME with
//! symbols and line tables so the report names who read the clock. Linux (`LD_PRELOAD`) and
//! macOS (`DYLD_INSERT_LIBRARIES`) only.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::artifacts::{Result, error};

/// Callers whose real clock reads never reach a run (simvar's `SIMULATOR_CLOCK_READS_ALLOW`,
/// to which the environment's own patterns are added):
///
/// * `foldhash` seeding its process-wide hashers once (the clock is one of its seed's
///   ingredients): the determinism check forbids iterating hash maps in simulated code, so a
///   map's seed decides where its entries sit, never what a lookup finds.
const ALLOWED: &[&str] = &["/foldhash-"];

/// Run the check for simulation `name` in the simulation workspace at `simulation`.
pub fn check(cargo: &std::ffi::OsStr, simulation: &Path, name: &str, args: &[String]) -> Result<()> {
    let (preload, extension) = if cfg!(target_os = "linux") {
        ("LD_PRELOAD", "so")
    } else if cfg!(target_os = "macos") {
        ("DYLD_INSERT_LIBRARIES", "dylib")
    } else {
        return Err(error("--clock-reads runs on Linux and macOS only"));
    };
    let target = simulation
        .parent()
        .unwrap_or(simulation)
        .join("target")
        .join("sim-clock-reads");
    let with_symbols = |command: &mut Command| {
        command
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_PROFILE_RELEASE_STRIP", "none")
            .env("CARGO_PROFILE_RELEASE_DEBUG", "line-tables-only");
    };

    let probe = probe_manifest(cargo, simulation)?;
    let mut build = Command::new(cargo);
    build
        .args(["build", "--release", "--package", "switchy_clock_probe", "--manifest-path"])
        .arg(&probe);
    with_symbols(&mut build);
    if !build.status()?.success() {
        return Err(error("building switchy_clock_probe failed"));
    }

    let mut build = Command::new(cargo);
    build
        // In simulation/, so cargo reads simulation/.cargo/config.toml, as `cargo sim` does.
        .current_dir(simulation)
        .args(["build", "--release", "--package", name]);
    with_symbols(&mut build);
    if !build.status()?.success() {
        return Err(error(format!("building {name} failed")));
    }

    let library = target
        .join("release")
        .join(format!("libswitchy_clock_probe.{extension}"));
    let mut run = Command::new(target.join("release").join(name));
    run.current_dir(simulation).args(args).env(preload, &library);
    if std::env::var_os("SIMULATOR_CLOCK_READS").is_none() {
        run.env("SIMULATOR_CLOCK_READS", "report");
    }
    let allowed: Vec<String> = ALLOWED
        .iter()
        .map(|pattern| (*pattern).to_owned())
        .chain(std::env::var("SIMULATOR_CLOCK_READS_ALLOW").ok())
        .filter(|pattern| !pattern.is_empty())
        .collect();
    run.env("SIMULATOR_CLOCK_READS_ALLOW", allowed.join(","));
    let status = run.status()?;
    if status.success() {
        Ok(())
    } else {
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// The manifest of the MoosicBox workspace the simulation workspace builds simvar from: the
/// first directory above `simvar_harness`'s that holds `switchy_clock_probe`.
fn probe_manifest(cargo: &std::ffi::OsStr, simulation: &Path) -> Result<PathBuf> {
    let output = Command::new(cargo)
        .current_dir(simulation)
        .args(["metadata", "--format-version", "1"])
        .output()?;
    if !output.status.success() {
        return Err(error(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| error(format!("cargo metadata printed no JSON: {e}")))?;
    let harness = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .and_then(|packages| {
            packages
                .iter()
                .find(|package| package.get("name") == Some(&"simvar_harness".into()))
        })
        .and_then(|package| package.get("manifest_path"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| error("the simulation workspace does not build simvar_harness"))?;
    Path::new(harness)
        .ancestors()
        .find(|dir| {
            dir.join("packages/switchy/clock_probe/Cargo.toml")
                .is_file()
        })
        .map(|root| root.join("Cargo.toml"))
        .ok_or_else(|| {
            error("simvar's MoosicBox checkout has no switchy_clock_probe (update MoosicBox)")
        })
}
