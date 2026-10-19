//! Repository tasks, deliberately outside build.rs and the runtime.
mod artifacts;
mod bterm;
mod bterm_run;
mod build;

use artifacts::{Result, error};
use std::{collections::BTreeMap, env, path::PathBuf};

fn main() {
    let args = invocation_args(env::args().next().as_deref(), env::args().skip(1).collect());
    if let Err(error) = run(args) {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}

fn invocation_args(executable: Option<&str>, mut args: Vec<String>) -> Vec<String> {
    if executable
        .and_then(|name| PathBuf::from(name).file_stem().map(|s| s.to_owned()))
        .is_some_and(|name| name == "bmux-install")
        && args.first().is_none_or(|command| command != "install")
    {
        args.insert(0, "install".into());
    }
    args
}

fn run(args: Vec<String>) -> Result<()> {
    let Some(command) = args.first() else {
        return Err(error(
            "usage: cargo xtask build [--target TARGET] [--profile PROFILE] | auxiliary {build,stage,nfpm} --target TARGET | bterm {build,run,stage,verify} --target TARGET | install SOURCE DESTINATION [--binary-name NAME | --activate-version VERSION]",
        ));
    };
    let mut positional = Vec::new();
    let mut options = BTreeMap::new();
    let mut index = 1;
    while index < args.len() {
        if args[index].starts_with("--") {
            let key = args[index].clone();
            let value = args
                .get(index + 1)
                .filter(|s| !s.starts_with("--"))
                .ok_or_else(|| error("flag requires a value"))?;
            if options.insert(key, value.clone()).is_some() {
                return Err(error("duplicate option"));
            }
            index += 2;
        } else {
            positional.push(args[index].clone());
            index += 1;
        }
    }
    let allowed: &[&str] = match command.as_str() {
        "bterm" => &[
            "--workspace",
            "--target",
            "--artifact-dir",
            "--destination",
            "--root",
        ],
        "build" => &["--workspace", "--target", "--profile"],
        "auxiliary" => &[
            "--workspace",
            "--target",
            "--artifact-dir",
            "--destination",
            "--config",
        ],
        "install" => &[
            "--binary-name",
            "--activate-version",
            "--version",
            "--prefix",
        ],
        _ => return Err(error("unknown task")),
    };
    if options.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(error("unknown option"));
    }
    if command == "bterm" {
        let required = |name: &str| {
            options
                .get(name)
                .map(PathBuf::from)
                .ok_or_else(|| error(format!("{name} is required")))
        };
        if matches!(positional.as_slice(), [operation] if operation == "build" || operation == "run")
        {
            if options.keys().any(|key| key != "--workspace") {
                return Err(error(
                    "bterm build/run detects its host and paths; only --workspace is accepted",
                ));
            }
            let workspace = options
                .get("--workspace")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));
            return if positional[0] == "build" {
                bterm_run::build(&workspace).map(|_| ())
            } else {
                bterm_run::run(&workspace)
            };
        }
        let target = options
            .get("--target")
            .ok_or_else(|| error("--target is required"))?;
        return match positional.as_slice() {
            [operation] if operation == "stage" => bterm::stage(
                &options
                    .get("--workspace")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")),
                &required("--artifact-dir")?,
                &required("--destination")?,
                target,
            ),
            [operation] if operation == "verify" => bterm::verify(&required("--root")?, target),
            _ => Err(error("bterm requires build, run, stage or verify")),
        };
    }
    if command == "build" {
        if !positional.is_empty() {
            return Err(error("build takes no positional arguments"));
        }
        let workspace = options
            .get("--workspace")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));
        let target = options
            .get("--target")
            .cloned()
            .map_or_else(|| build::host_target(&workspace), Ok)?;
        let profile = options.get("--profile").map_or("dev", String::as_str);
        return build::run(
            &workspace,
            &target,
            profile,
            &artifacts::declarations(&workspace)?,
        );
    }
    if command == "install" {
        let source = positional
            .first()
            .ok_or_else(|| error("install requires SOURCE"))?;
        let destination = match (positional.get(1), options.get("--prefix")) {
            (Some(value), None) | (None, Some(value)) => PathBuf::from(value),
            _ => return Err(error("specify DESTINATION or --prefix, not both")),
        };
        if positional.len() > 2 {
            return Err(error("too many arguments"));
        }
        if options.contains_key("--version") && options.contains_key("--activate-version") {
            return Err(error("duplicate version"));
        }
        let version = options
            .get("--activate-version")
            .or_else(|| options.get("--version"));
        let binary = options.get("--binary-name").map(String::as_str);
        if let Some(version) = version {
            if binary.is_some() {
                return Err(error("activation and slot rename are mutually exclusive"));
            }
            artifacts::activate(source.as_ref(), &destination, version)
        } else {
            artifacts::install(source.as_ref(), &destination, binary)
        }
    } else {
        if positional.len() != 1 {
            return Err(error("auxiliary requires one operation"));
        }
        let workspace = options
            .get("--workspace")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));
        let target = options
            .get("--target")
            .ok_or_else(|| error("--target is required"))?;
        let unix = artifacts::unix_target(target)?;
        let declarations = artifacts::declarations(&workspace)?;
        let required = |name: &str| {
            options
                .get(name)
                .map(PathBuf::from)
                .ok_or_else(|| error(format!("{name} is required")))
        };
        match positional[0].as_str() {
            "build" => build::run(&workspace, target, "release", &declarations),
            "stage" => artifacts::stage(
                &required("--artifact-dir")?,
                &required("--destination")?,
                target,
                &declarations,
            ),
            "nfpm" => {
                if !unix {
                    return Err(error("nfpm requires a Unix target"));
                }
                artifacts::nfpm(
                    &required("--config")?,
                    &required("--destination")?,
                    &declarations,
                )
            }
            _ => Err(error("unknown auxiliary operation")),
        }
    }
}

#[cfg(test)]
mod tests;
