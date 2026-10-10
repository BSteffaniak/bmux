//! Repository tasks, deliberately outside build.rs and the runtime.
mod artifacts;

use artifacts::{Result, error};
use std::{collections::BTreeMap, env, path::PathBuf, process::Command};

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
            "usage: cargo xtask auxiliary {build,stage,nfpm} --target TARGET | install SOURCE DESTINATION [--binary-name NAME | --activate-version VERSION]",
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
            "build" => {
                let status = Command::new("cargo")
                    .args([
                        "build",
                        "--locked",
                        "--release",
                        "--target",
                        target,
                        "-p",
                        "xtask",
                        "--bin",
                        "xtask",
                    ])
                    .current_dir(&workspace)
                    .status()?;
                if !status.success() {
                    return Err(error("installer build failed"));
                }
                if unix {
                    for item in declarations {
                        let status = Command::new("cargo")
                            .args([
                                "build",
                                "--locked",
                                "--release",
                                "--target",
                                target,
                                "-p",
                                artifacts::field(&item, "cargo_package")?,
                                "--bin",
                                artifacts::field(&item, "cargo_binary")?,
                            ])
                            .current_dir(&workspace)
                            .status()?;
                        if !status.success() {
                            return Err(error("auxiliary build failed"));
                        }
                    }
                }
                Ok(())
            }
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
