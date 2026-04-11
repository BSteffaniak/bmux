#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::{
    CoreCliCommandRequest, CoreCliCommandResponse, EXIT_OK, NativeCommandContext,
    PluginCliCommandRequest, PluginCliCommandResponse, PluginCommandError, RustPlugin,
};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

#[derive(Default)]
pub struct PluginCliPlugin;

impl RustPlugin for PluginCliPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        match context.command.as_str() {
            "list" => run_list_command(&context).map_err(PluginCommandError::from),
            "run" => run_run_command(&context).map_err(PluginCommandError::from),
            "rebuild" => run_rebuild_command(&context).map_err(PluginCommandError::from),
            _ => {
                if let Some(command_path) = core_proxy_command_path(context.command.as_str()) {
                    run_core_proxy_command(&context, command_path)
                } else {
                    Err(PluginCommandError::from(format!(
                        "unsupported command '{}'",
                        context.command
                    )))
                }
            }
        }
    }
}

struct CoreProxyCommand {
    command_name: &'static str,
    command_path: &'static [&'static str],
}

const CORE_PROXY_COMMANDS: &[CoreProxyCommand] = &[
    CoreProxyCommand {
        command_name: "logs-path",
        command_path: &["logs", "path"],
    },
    CoreProxyCommand {
        command_name: "logs-level",
        command_path: &["logs", "level"],
    },
    CoreProxyCommand {
        command_name: "logs-tail",
        command_path: &["logs", "tail"],
    },
    CoreProxyCommand {
        command_name: "logs-watch",
        command_path: &["logs", "watch"],
    },
    CoreProxyCommand {
        command_name: "logs-profiles-list",
        command_path: &["logs", "profiles", "list"],
    },
    CoreProxyCommand {
        command_name: "logs-profiles-show",
        command_path: &["logs", "profiles", "show"],
    },
    CoreProxyCommand {
        command_name: "logs-profiles-delete",
        command_path: &["logs", "profiles", "delete"],
    },
    CoreProxyCommand {
        command_name: "logs-profiles-rename",
        command_path: &["logs", "profiles", "rename"],
    },
    CoreProxyCommand {
        command_name: "keymap-doctor",
        command_path: &["keymap", "doctor"],
    },
    CoreProxyCommand {
        command_name: "terminal-doctor",
        command_path: &["terminal", "doctor"],
    },
    CoreProxyCommand {
        command_name: "terminal-install-terminfo",
        command_path: &["terminal", "install-terminfo"],
    },
    CoreProxyCommand {
        command_name: "recording-start",
        command_path: &["recording", "start"],
    },
    CoreProxyCommand {
        command_name: "recording-stop",
        command_path: &["recording", "stop"],
    },
    CoreProxyCommand {
        command_name: "recording-status",
        command_path: &["recording", "status"],
    },
    CoreProxyCommand {
        command_name: "recording-path",
        command_path: &["recording", "path"],
    },
    CoreProxyCommand {
        command_name: "recording-list",
        command_path: &["recording", "list"],
    },
    CoreProxyCommand {
        command_name: "recording-delete",
        command_path: &["recording", "delete"],
    },
    CoreProxyCommand {
        command_name: "recording-delete-all",
        command_path: &["recording", "delete-all"],
    },
    CoreProxyCommand {
        command_name: "recording-cut",
        command_path: &["recording", "cut"],
    },
    CoreProxyCommand {
        command_name: "recording-inspect",
        command_path: &["recording", "inspect"],
    },
    CoreProxyCommand {
        command_name: "recording-replay",
        command_path: &["recording", "replay"],
    },
    CoreProxyCommand {
        command_name: "recording-verify-smoke",
        command_path: &["recording", "verify-smoke"],
    },
    CoreProxyCommand {
        command_name: "recording-export",
        command_path: &["recording", "export"],
    },
    CoreProxyCommand {
        command_name: "recording-prune",
        command_path: &["recording", "prune"],
    },
    CoreProxyCommand {
        command_name: "playbook-run",
        command_path: &["playbook", "run"],
    },
    CoreProxyCommand {
        command_name: "playbook-validate",
        command_path: &["playbook", "validate"],
    },
    CoreProxyCommand {
        command_name: "playbook-interactive",
        command_path: &["playbook", "interactive"],
    },
    CoreProxyCommand {
        command_name: "playbook-from-recording",
        command_path: &["playbook", "from-recording"],
    },
    CoreProxyCommand {
        command_name: "playbook-dry-run",
        command_path: &["playbook", "dry-run"],
    },
    CoreProxyCommand {
        command_name: "playbook-diff",
        command_path: &["playbook", "diff"],
    },
    CoreProxyCommand {
        command_name: "playbook-cleanup",
        command_path: &["playbook", "cleanup"],
    },
];

fn core_proxy_command_path(command_name: &str) -> Option<&'static [&'static str]> {
    CORE_PROXY_COMMANDS
        .iter()
        .find(|entry| entry.command_name == command_name)
        .map(|entry| entry.command_path)
}

fn run_core_proxy_command(
    context: &NativeCommandContext,
    command_path: &[&str],
) -> Result<i32, PluginCommandError> {
    let request = CoreCliCommandRequest::new(
        command_path.iter().map(ToString::to_string).collect(),
        context.arguments.clone(),
    );
    let response: CoreCliCommandResponse =
        context
            .core_cli_command_run_path(&request)
            .map_err(|error| {
                PluginCommandError::from(format!(
                    "failed running core command path via host bridge: {error}"
                ))
            })?;
    Ok(response.exit_code)
}

fn run_list_command(context: &NativeCommandContext) -> Result<i32, String> {
    let as_json = has_flag(&context.arguments, "json");
    let enabled = context.enabled_plugins.iter().collect::<BTreeSet<_>>();

    let mut entries = context
        .registered_plugins
        .iter()
        .map(|plugin| PluginListEntry {
            id: plugin.id.clone(),
            display_name: plugin.display_name.clone(),
            version: plugin.version.clone(),
            enabled: enabled.contains(&plugin.id),
            required_capabilities: plugin.required_capabilities.clone(),
            provided_capabilities: plugin.provided_capabilities.clone(),
            commands: plugin.commands.clone(),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.id.cmp(&right.id));

    if as_json {
        let output = serde_json::to_string_pretty(&entries)
            .map_err(|error| format!("failed encoding plugin list json: {error}"))?;
        println!("{output}");
        return Ok(EXIT_OK);
    }

    if entries.is_empty() {
        println!("no plugins discovered");
        return Ok(EXIT_OK);
    }

    for entry in entries {
        println!(
            "{}{} - {} ({})",
            entry.id,
            if entry.enabled { " [enabled]" } else { "" },
            entry.display_name,
            entry.version
        );
        if !entry.commands.is_empty() {
            println!("  commands: {}", entry.commands.join(", "));
        }
        if !entry.required_capabilities.is_empty() {
            println!(
                "  required capabilities: {}",
                entry.required_capabilities.join(", ")
            );
        }
        if !entry.provided_capabilities.is_empty() {
            println!(
                "  provided capabilities: {}",
                entry.provided_capabilities.join(", ")
            );
        }
    }

    Ok(EXIT_OK)
}

fn run_run_command(context: &NativeCommandContext) -> Result<i32, String> {
    if context.arguments.len() < 2 {
        return Err("usage: bmux plugin run <plugin> <command> [args ...]".to_string());
    }

    let plugin_id = context.arguments[0].clone();
    let command_name = context.arguments[1].clone();
    let args = context.arguments[2..].to_vec();

    if plugin_id == context.plugin_id {
        return Err(
            "running 'bmux.plugin_cli' via 'bmux plugin run' is not supported (self-invocation deadlock guard)"
                .to_string(),
        );
    }

    let request = PluginCliCommandRequest::new(plugin_id.clone(), command_name.clone(), args);
    let response: PluginCliCommandResponse = context
        .plugin_command_run(&request)
        .map_err(|error| format_plugin_command_run_error(&plugin_id, &command_name, &error))?;
    Ok(response.exit_code)
}

fn run_rebuild_command(context: &NativeCommandContext) -> Result<i32, String> {
    let options = parse_rebuild_options(&context.arguments)?;
    let metadata = cargo_metadata()?;
    let workspace_plugin_crates = workspace_plugin_cdylib_crates(&metadata);
    let bundled_plugins = discover_bundled_plugins(context)?;

    if options.list_only {
        print_discovered_plugins(&bundled_plugins, &workspace_plugin_crates);
        return Ok(EXIT_OK);
    }

    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add_target = |crate_name: &str| {
        if seen.insert(crate_name.to_string()) {
            targets.push(crate_name.to_string());
        }
    };

    if options.selectors.is_empty() {
        if options.all_workspace_plugins {
            for crate_name in &workspace_plugin_crates {
                add_target(crate_name);
            }
        } else {
            for bundled in &bundled_plugins {
                add_target(&bundled.crate_name);
            }
        }
    } else {
        for selector in &options.selectors {
            let resolved = resolve_selector(selector, &bundled_plugins, &workspace_plugin_crates)?;
            add_target(&resolved);
        }
    }

    if targets.is_empty() {
        return Err(
            "no plugin crates selected to build; use --all-workspace-plugins or provide selectors"
                .to_string(),
        );
    }

    for crate_name in &targets {
        if !workspace_plugin_crates
            .iter()
            .any(|known| known == crate_name)
        {
            return Err(format!(
                "selected crate '{crate_name}' is not a workspace plugin cdylib crate"
            ));
        }
    }

    println!(
        "building plugin crates ({}): {}",
        if options.release { "release" } else { "debug" },
        targets.join(" ")
    );

    let mut command = ProcessCommand::new("cargo");
    command.arg("build");
    if options.release {
        command.arg("--release");
    }
    for crate_name in &targets {
        command.arg("-p");
        command.arg(crate_name);
    }

    let status = command
        .status()
        .map_err(|error| format!("failed executing cargo build: {error}"))?;

    if !status.success() {
        return Err("cargo build failed for selected plugin crates".to_string());
    }

    Ok(EXIT_OK)
}

fn discover_bundled_plugins(context: &NativeCommandContext) -> Result<Vec<BundledPlugin>, String> {
    let mut discovered = Vec::new();
    let mut seen_ids = BTreeSet::new();

    for root in plugin_roots(context) {
        let Ok(report) = bmux_plugin::discover_plugin_manifests(&root) else {
            continue;
        };
        for manifest_path in report.manifest_paths {
            let manifest = bmux_plugin::PluginManifest::from_path(&manifest_path)
                .map_err(|error| format!("failed parsing {}: {error}", manifest_path.display()))?;
            if !seen_ids.insert(manifest.id.as_str().to_string()) {
                continue;
            }

            let short_name = manifest_path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    format!(
                        "invalid plugin path for manifest {}",
                        manifest_path.display()
                    )
                })?
                .to_string();
            let crate_name = manifest
                .entry
                .as_ref()
                .and_then(|e| entry_to_crate_name(e))
                .unwrap_or_else(|| dir_name_to_crate_name(&short_name));

            discovered.push(BundledPlugin {
                plugin_id: manifest.id.as_str().to_string(),
                short_name,
                crate_name,
            });
        }
    }

    discovered.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
    Ok(discovered)
}

fn print_discovered_plugins(bundled: &[BundledPlugin], workspace_crates: &[String]) {
    println!("bundled plugins:");
    if bundled.is_empty() {
        println!("  (none discovered)");
    } else {
        for entry in bundled {
            println!(
                "  - {} short={} crate={}",
                entry.plugin_id, entry.short_name, entry.crate_name
            );
        }
    }

    println!("workspace plugin crates:");
    if workspace_crates.is_empty() {
        println!("  (none)");
    } else {
        for crate_name in workspace_crates {
            println!("  - {crate_name}");
        }
    }
}

fn resolve_selector(
    selector: &str,
    bundled: &[BundledPlugin],
    workspace_crates: &[String],
) -> Result<String, String> {
    if workspace_crates
        .iter()
        .any(|crate_name| crate_name == selector)
    {
        return Ok(selector.to_string());
    }
    if let Some(entry) = bundled.iter().find(|entry| entry.plugin_id == selector) {
        return Ok(entry.crate_name.clone());
    }
    if let Some(entry) = bundled.iter().find(|entry| entry.short_name == selector) {
        return Ok(entry.crate_name.clone());
    }

    let known_ids = bundled
        .iter()
        .map(|entry| entry.plugin_id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let known_short = bundled
        .iter()
        .map(|entry| entry.short_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let known_crates = workspace_crates.join(", ");

    Err(format!(
        "unknown plugin selector '{selector}'\nknown bundled ids: {known_ids}\nknown bundled short names: {known_short}\nknown workspace plugin crates: {known_crates}"
    ))
}

fn cargo_metadata() -> Result<serde_json::Value, String> {
    let output = ProcessCommand::new("cargo")
        .arg("metadata")
        .arg("--no-deps")
        .arg("--format-version")
        .arg("1")
        .output()
        .map_err(|error| {
            format!(
                "failed executing 'cargo metadata': {error}. If this environment cannot use cargo metadata, run direct cargo build commands for your plugin crates."
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "'cargo metadata' failed: {}. Run direct cargo build commands for your plugin crates if needed.",
            stderr.trim()
        ));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("failed parsing cargo metadata json: {error}"))
}

fn workspace_plugin_cdylib_crates(metadata: &serde_json::Value) -> Vec<String> {
    let mut crates = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|package| {
            let name = package.get("name")?.as_str()?.to_string();
            let manifest_path = package.get("manifest_path")?.as_str()?.to_string();
            if !manifest_path.contains("/plugins/") {
                return None;
            }
            let has_cdylib = package
                .get("targets")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|targets| {
                    targets.iter().any(|target| {
                        target
                            .get("crate_types")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|types| {
                                types.iter().any(|crate_type| crate_type == "cdylib")
                            })
                    })
                });
            has_cdylib.then_some(name)
        })
        .collect::<Vec<_>>();
    crates.sort();
    crates
}

fn parse_rebuild_options(arguments: &[String]) -> Result<RebuildOptions, String> {
    let mut options = RebuildOptions::default();
    let mut positional_mode = false;

    for argument in arguments {
        if argument == "--" {
            positional_mode = true;
            continue;
        }

        if !positional_mode {
            match argument.as_str() {
                "--release" => {
                    options.release = true;
                    continue;
                }
                "--list" => {
                    options.list_only = true;
                    continue;
                }
                "--all-workspace-plugins" => {
                    options.all_workspace_plugins = true;
                    continue;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown option: {value}"));
                }
                _ => {}
            }
        }

        options.selectors.push(argument.clone());
    }

    Ok(options)
}

fn plugin_roots(context: &NativeCommandContext) -> Vec<PathBuf> {
    context
        .plugin_search_roots
        .iter()
        .map(PathBuf::from)
        .collect()
}

fn entry_to_crate_name(entry: &Path) -> Option<String> {
    let mut file_name = entry.file_name()?.to_str()?.to_string();
    if file_name.starts_with("lib") {
        file_name = file_name[3..].to_string();
    }
    let dot = file_name.find('.')?;
    Some(file_name[..dot].to_string())
}

/// Derive a crate name from a plugin directory name by prefixing `bmux_` and
/// replacing hyphens with underscores.  For example, `clipboard-plugin` becomes
/// `bmux_clipboard_plugin`.
fn dir_name_to_crate_name(dir_name: &str) -> String {
    format!("bmux_{}", dir_name.replace('-', "_"))
}

fn has_flag(arguments: &[String], long_name: &str) -> bool {
    let long_flag = format!("--{long_name}");
    arguments.iter().any(|argument| argument == &long_flag)
}

fn format_plugin_command_run_error(
    plugin_id: &str,
    command_name: &str,
    error: &dyn std::fmt::Display,
) -> String {
    let base = format!("failed running plugin command '{plugin_id}:{command_name}': {error}");
    if base.contains("session policy denied for this operation") {
        format!(
            "{base}\nHint: operation denied by an active policy provider. Verify policy state or run with an authorized principal."
        )
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::CORE_PROXY_COMMANDS;
    use std::collections::BTreeSet;

    #[test]
    fn proxy_command_table_stays_in_sync_with_manifest_commands() {
        let manifest = include_str!("../plugin.toml");
        let proxy_names = CORE_PROXY_COMMANDS
            .iter()
            .map(|entry| entry.command_name.to_string())
            .collect::<BTreeSet<_>>();
        let manifest_proxy_names = manifest_proxy_command_names(manifest);
        assert_eq!(
            proxy_names, manifest_proxy_names,
            "core proxy command table must match non-plugin command declarations in plugin.toml"
        );
    }

    fn manifest_proxy_command_names(manifest: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut current_name: Option<String> = None;
        let mut in_command = false;

        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed == "[[commands]]" {
                in_command = true;
                current_name = None;
                continue;
            }
            if trimmed.starts_with("[[commands.") {
                in_command = false;
                continue;
            }
            if !in_command {
                continue;
            }

            if let Some(value) = parse_quoted_value(trimmed, "name") {
                current_name = Some(value.to_string());
                continue;
            }

            if let Some(path_text) = parse_array_text(trimmed, "path")
                && let Some(name) = current_name.take()
                && !path_text.contains("\"plugin\"")
            {
                names.insert(name);
            }
        }

        names
    }

    fn parse_quoted_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let (left, right) = line.split_once('=')?;
        if left.trim() != key {
            return None;
        }
        let value = right.trim();
        if !value.starts_with('"') || !value.ends_with('"') || value.len() < 2 {
            return None;
        }
        Some(&value[1..value.len() - 1])
    }

    fn parse_array_text<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let (left, right) = line.split_once('=')?;
        if left.trim() != key {
            return None;
        }
        let value = right.trim();
        if !value.starts_with('[') || !value.ends_with(']') {
            return None;
        }
        Some(value)
    }
}

#[derive(Debug, Default)]
struct RebuildOptions {
    release: bool,
    list_only: bool,
    all_workspace_plugins: bool,
    selectors: Vec<String>,
}

#[derive(Debug)]
struct BundledPlugin {
    plugin_id: String,
    short_name: String,
    crate_name: String,
}

#[derive(Debug, Serialize)]
struct PluginListEntry {
    id: String,
    display_name: String,
    version: String,
    enabled: bool,
    required_capabilities: Vec<String>,
    provided_capabilities: Vec<String>,
    commands: Vec<String>,
}

bmux_plugin_sdk::export_plugin!(PluginCliPlugin, include_str!("../plugin.toml"));
