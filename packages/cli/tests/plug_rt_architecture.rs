//! One Tokio runtime per host process: plugin-side code never builds one.
//!
//! Plugin code reaches the host runtime through plug-rt
//! (`bmux_plugin_sdk::rt`). A plugin that still calls Tokio's runtime APIs
//! directly cannot run as a dynamic library, so it must declare
//! `bundled_only` and the dynamic loader refuses it with a typed error.
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Production Rust sources under `dir`: test files, `tests/` directories and
/// trailing `mod tests` blocks are excluded.
fn production_sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name == "target" || name == "tests" || name.starts_with('.') {
                continue;
            }
            production_sources(&path, out);
        } else if name.ends_with(".rs")
            && name != "tests.rs"
            && !name.ends_with("_tests.rs")
            && let Ok(source) = std::fs::read_to_string(&path)
        {
            let production = source
                .split("\n#[cfg(test)]\nmod tests")
                .next()
                .unwrap_or(&source)
                .split("\nmod tests {")
                .next()
                .unwrap_or(&source)
                .to_string();
            out.push((path, production));
        }
    }
}

/// Crates whose code is linked into plugins (bundled or dynamic).
fn plugin_side_crates() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut crates = vec![
        root.join("packages/plugin-sdk"),
        root.join("packages/plugin-runtime"),
    ];
    for parent in ["plugins", "examples"] {
        let Ok(entries) = std::fs::read_dir(root.join(parent)) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().join("plugin.toml").is_file() {
                crates.push(entry.path());
            }
        }
    }
    crates.sort();
    crates
}

const RUNTIME_CONSTRUCTION: [&str; 3] = [
    "Builder::new_current_thread(",
    "Builder::new_multi_thread(",
    "tokio::runtime::Runtime::new(",
];

/// Remaining legacy executors built by synchronous command and service bridges in
/// bundled-only plugins, with their exact counts. This list may only shrink:
/// porting a command to an async entry on plug-rt removes its entry.
const LEGACY_SYNC_COMMAND_EXECUTORS: [(&str, usize); 7] = [
    ("plugins/broadcast-plugin/src/controller.rs", 2),
    ("plugins/broadcast-plugin/src/lib.rs", 1),
    ("plugins/presenter-plugin/src/lib.rs", 1),
    ("plugins/presenter-plugin/src/services.rs", 1),
    ("plugins/recording-plugin/src/interactive_command.rs", 1),
    ("plugins/recording-plugin/src/process_commands.rs", 1),
    ("plugins/recording-plugin/src/process_runtime.rs", 1),
];

#[test]
fn plugin_code_never_builds_a_runtime() {
    let root = workspace_root();
    let mut observed: Vec<(String, usize)> = Vec::new();
    for krate in plugin_side_crates() {
        let mut sources = Vec::new();
        production_sources(&krate.join("src"), &mut sources);
        for (path, source) in sources {
            let count: usize = RUNTIME_CONSTRUCTION
                .iter()
                .map(|marker| source.matches(marker).count())
                .sum();
            if count > 0 {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
                    .replace("../", "");
                observed.push((relative, count));
            }
        }
    }
    let mut allowed: Vec<(String, usize)> = LEGACY_SYNC_COMMAND_EXECUTORS
        .iter()
        .map(|(path, count)| ((*path).to_string(), *count))
        .collect();
    observed.sort();
    allowed.sort();
    assert_eq!(
        observed, allowed,
        "plugin-side code must use plug-rt (bmux_plugin_sdk::rt) instead of building a Tokio \
         runtime; the legacy list may only shrink",
    );
}

/// Tokio APIs that need a runtime context in the calling image. Inside a
/// dynamic library they find none and panic.
const RUNTIME_BOUND_TOKIO: [&str; 10] = [
    "tokio::spawn(",
    "tokio::time::",
    "tokio::task::spawn",
    "tokio::task::block_in_place",
    "tokio::net::",
    "tokio::process::",
    "Handle::current()",
    "Handle::try_current()",
    "Builder::new_current_thread(",
    "Builder::new_multi_thread(",
];

#[test]
fn plugins_calling_tokio_runtime_apis_are_bundled_only() {
    let mut undeclared = Vec::new();
    for krate in plugin_side_crates() {
        let Ok(manifest) = std::fs::read_to_string(krate.join("plugin.toml")) else {
            continue;
        };
        let declared = manifest
            .lines()
            .any(|line| line.trim_start().starts_with("bundled_only"));
        let mut sources = Vec::new();
        production_sources(&krate.join("src"), &mut sources);
        let uses_tokio_runtime = sources.iter().any(|(_, source)| {
            RUNTIME_BOUND_TOKIO
                .iter()
                .any(|marker| source.contains(marker))
        });
        if uses_tokio_runtime && !declared {
            undeclared.push(krate.display().to_string());
        }
    }
    assert!(
        undeclared.is_empty(),
        "plugins that call Tokio runtime APIs directly cannot load as dynamic libraries; port \
         them to plug-rt or declare `bundled_only = \"<reason>\"` in plugin.toml: \
         {undeclared:#?}",
    );
}
