//! `cargo sim NAME --check`: check that everything simulation NAME links runs deterministically
//! under it, with `switchy_cargo`: every local crate it links scanned by the rules in its
//! manifest's `[package.metadata.switchy.simulation]`, and every other crate on the audit list
//! in `simulation/determinism.toml`.
//!
//! `sim-check NAME... --coverage EXPORT` (run by `cargo sim NAME --coverage`) checks the
//! claims that crates NAME links never run (local crates' `runs = false`, external crates'
//! `not-run` audits) instead, against an `llvm-cov export` of its coverage-instrumented runs.
//!
//! A separate program, so the check's own dependencies stay out of the simulations' graphs.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::{path::Path, process::ExitCode};

fn main() -> ExitCode {
    let mut names: Vec<String> = std::env::args().skip(1).collect();
    let coverage = names.iter().position(|arg| arg == "--coverage").map(|at| {
        let export = names.get(at + 1).cloned();
        names.drain(at..(at + 2).min(names.len()));
        export
    });
    if names.is_empty() || coverage.as_ref().is_some_and(Option::is_none) {
        eprintln!(
            "usage: sim-check NAME... [--coverage EXPORT]; or cargo sim NAME --check, \
             cargo sim NAME --coverage"
        );
        return ExitCode::FAILURE;
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| Path::new("Cargo.toml").to_path_buf(), |dir| dir.join("Cargo.toml"));
    if let Some(Some(export)) = coverage {
        return check_claims(&manifest, &names, Path::new(&export));
    }
    let mut passed = true;
    for name in names {
        match switchy_cargo::Check::with_workspace_config(&manifest, &name)
            .and_then(|check| check.run())
        {
            Ok(report) => {
                println!("== {name}\n{report}");
                passed &= report.passed();
            }
            Err(error) => {
                eprintln!("{name}: {error}");
                passed = false;
            }
        }
    }
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Check the claims that crates each simulation links never run against the coverage export.
fn check_claims(manifest: &Path, names: &[String], export: &Path) -> ExitCode {
    let executed = match std::fs::read_to_string(export)
        .map_err(|e| e.to_string())
        .and_then(|json| switchy_cargo::coverage::Executed::from_llvm_cov_export(&json))
    {
        Ok(executed) => executed,
        Err(error) => {
            eprintln!("{}: {error}", export.display());
            return ExitCode::FAILURE;
        }
    };
    let mut passed = true;
    for name in names {
        match switchy_cargo::Check::with_workspace_config(manifest, name)
            .and_then(|check| check.run_claims(&executed))
        {
            Ok(broken) => {
                for (krate, files) in &broken {
                    println!("{name}: {krate} is claimed never to run, but ran:");
                    for (file, lines) in files {
                        println!("  {} ({lines} lines)", file.display());
                    }
                }
                passed &= broken.is_empty();
            }
            Err(error) => {
                eprintln!("{name}: {error}");
                passed = false;
            }
        }
    }
    if passed {
        println!(
            "claims that crates never run held ({} source files in the profile)",
            executed.files()
        );
        ExitCode::SUCCESS
    } else {
        println!(
            "claims that crates never run BROKEN: drop the claim (a local crate's `runs = false`, \
             so the check scans it; an external crate's `not-run` audit, for a real verdict) and \
             make what runs deterministic"
        );
        ExitCode::FAILURE
    }
}
