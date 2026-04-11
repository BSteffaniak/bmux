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
            "doctor" => run_doctor_command(&context).map_err(PluginCommandError::from),
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

include!(concat!(env!("OUT_DIR"), "/core_proxy_commands.rs"));

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

fn run_doctor_command(context: &NativeCommandContext) -> Result<i32, String> {
    let as_json = has_flag(&context.arguments, "json");
    let enabled_ids = context
        .enabled_plugins
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let enabled_plugins = context
        .registered_plugins
        .iter()
        .filter(|plugin| enabled_ids.contains(&plugin.id))
        .collect::<Vec<_>>();

    let mut issues = Vec::new();
    for plugin_id in &context.enabled_plugins {
        if !context
            .registered_plugins
            .iter()
            .any(|registered| registered.id == *plugin_id)
        {
            issues.push(format!(
                "enabled plugin '{plugin_id}' was not found in the registry"
            ));
        }
    }

    let available_capabilities = context
        .available_capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for plugin in &enabled_plugins {
        for required in &plugin.required_capabilities {
            if !available_capabilities.contains(required) {
                issues.push(format!(
                    "plugin '{}' requires unavailable capability '{}'",
                    plugin.id, required
                ));
            }
        }
    }

    let report = PluginDoctorReport {
        healthy: issues.is_empty(),
        enabled_plugins: context.enabled_plugins.clone(),
        issues,
    };

    if as_json {
        let output = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed encoding plugin doctor json: {error}"))?;
        println!("{output}");
    } else if report.healthy {
        println!(
            "plugin doctor: ok ({} enabled plugins)",
            report.enabled_plugins.len()
        );
    } else {
        println!("plugin doctor: found {} issue(s)", report.issues.len());
        for issue in &report.issues {
            println!("- {issue}");
        }
    }

    Ok(if report.healthy { EXIT_OK } else { 1 })
}

fn run_run_command(context: &NativeCommandContext) -> Result<i32, String> {
    if context.arguments.len() < 2 {
        return Err("usage: bmux plugin run <plugin> <command> [args ...]".to_string());
    }

    let plugin_id = context.arguments[0].clone();
    let command_name = context.arguments[1].clone();
    let args = context.arguments[2..].to_vec();

    let available_ids = context
        .registered_plugins
        .iter()
        .map(|plugin| plugin.id.as_str())
        .collect::<Vec<_>>();
    let Some(target_plugin) = context
        .registered_plugins
        .iter()
        .find(|plugin| plugin.id == plugin_id)
    else {
        let suggestion = suggest_closest(&plugin_id, &available_ids);
        return Err(suggestion.map_or_else(
            || {
                format!(
                    "plugin '{plugin_id}' was not found. Run 'bmux plugin list --json' to inspect available plugins."
                )
            },
            |candidate| {
                format!(
                "plugin '{plugin_id}' was not found. Did you mean '{candidate}'? Run 'bmux plugin list --json' to inspect available plugins."
                )
            },
        ));
    };

    if !target_plugin
        .commands
        .iter()
        .any(|name| name == &command_name)
    {
        let known = target_plugin
            .commands
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let suggestion = suggest_closest(&command_name, &known);
        return Err(suggestion.map_or_else(
            || format!("plugin '{plugin_id}' does not declare command '{command_name}'."),
            |candidate| {
                format!(
                "plugin '{plugin_id}' does not declare command '{command_name}'. Did you mean '{candidate}'?"
                )
            },
        ));
    }

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

    if matches!(options.mode, RebuildMode::List) {
        if options.json {
            let report = RebuildSelectionReport {
                profile: rebuild_profile_name(options.release),
                selected_targets: Vec::new(),
                mode: "list".to_string(),
            };
            let output = serde_json::to_string_pretty(&report)
                .map_err(|error| format!("failed encoding rebuild json: {error}"))?;
            println!("{output}");
        }
        print_discovered_plugins(&bundled_plugins, &workspace_plugin_crates);
        return Ok(EXIT_OK);
    }

    let targets = select_rebuild_targets(&options, &bundled_plugins, &workspace_plugin_crates)?;

    if targets.is_empty() {
        if matches!(options.mode, RebuildMode::Changed) {
            return Err("no changed workspace plugin crates were detected".to_string());
        }
        return Err(
            "no plugin crates selected to build; provide one or more selectors".to_string(),
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

    if options.json {
        let report = RebuildSelectionReport {
            profile: rebuild_profile_name(options.release),
            selected_targets: targets.clone(),
            mode: rebuild_mode_name(&options),
        };
        let output = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed encoding rebuild json: {error}"))?;
        println!("{output}");
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

fn rebuild_profile_name(release: bool) -> String {
    if release {
        "release".to_string()
    } else {
        "debug".to_string()
    }
}

fn rebuild_mode_name(options: &RebuildOptions) -> String {
    if options.selectors.is_empty() {
        match options.mode {
            RebuildMode::List => "list".to_string(),
            RebuildMode::Changed => "changed".to_string(),
            RebuildMode::AllWorkspace => "all-workspace".to_string(),
        }
    } else {
        "selectors".to_string()
    }
}

fn select_rebuild_targets(
    options: &RebuildOptions,
    bundled_plugins: &[BundledPlugin],
    workspace_plugin_crates: &[String],
) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add_target = |crate_name: &str| {
        if seen.insert(crate_name.to_string()) {
            targets.push(crate_name.to_string());
        }
    };

    if !options.selectors.is_empty() {
        for selector in &options.selectors {
            let resolved = resolve_selector(selector, bundled_plugins, workspace_plugin_crates)?;
            add_target(&resolved);
        }
        return Ok(targets);
    }

    match options.mode {
        RebuildMode::Changed => {
            for crate_name in
                changed_workspace_plugin_crates(bundled_plugins, workspace_plugin_crates)?
            {
                add_target(&crate_name);
            }
        }
        RebuildMode::AllWorkspace | RebuildMode::List => {
            if options.all_workspace_plugins {
                println!("--all-workspace-plugins is now the default behavior");
            }
            for crate_name in workspace_plugin_crates {
                add_target(crate_name);
            }
        }
    }

    Ok(targets)
}

fn changed_workspace_plugin_crates(
    bundled: &[BundledPlugin],
    workspace_crates: &[String],
) -> Result<Vec<String>, String> {
    let changed_paths = collect_changed_paths()?;
    let mut selected = BTreeSet::new();

    if changed_paths.iter().any(|path| {
        path.starts_with("packages/plugin/") || path.starts_with("packages/plugin-sdk/")
    }) {
        return Ok(workspace_crates.to_vec());
    }

    for path in changed_paths {
        let mut segments = path.split('/');
        if segments.next() != Some("plugins") {
            continue;
        }
        let Some(short_name) = segments.next() else {
            continue;
        };
        if let Some(entry) = bundled.iter().find(|entry| entry.short_name == short_name)
            && workspace_crates
                .iter()
                .any(|name| name == &entry.crate_name)
        {
            selected.insert(entry.crate_name.clone());
            continue;
        }
        let derived = dir_name_to_crate_name(short_name);
        if workspace_crates.iter().any(|name| name == &derived) {
            selected.insert(derived);
        }
    }

    Ok(selected.into_iter().collect())
}

fn collect_changed_paths() -> Result<Vec<String>, String> {
    let mut changed = BTreeSet::new();

    for args in [
        ["diff", "--name-only", "--relative", "HEAD"].as_slice(),
        ["diff", "--name-only", "--relative", "--cached"].as_slice(),
        ["ls-files", "--others", "--exclude-standard"].as_slice(),
    ] {
        let output = ProcessCommand::new("git")
            .args(args)
            .output()
            .map_err(|error| format!("failed executing 'git {}': {error}", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "'git {}' failed: {}",
                args.join(" "),
                stderr.trim()
            ));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                changed.insert(trimmed.to_string());
            }
        }
    }

    Ok(changed.into_iter().collect())
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
                    options.mode = RebuildMode::List;
                    continue;
                }
                "--json" => {
                    options.json = true;
                    continue;
                }
                "--changed" => {
                    options.mode = RebuildMode::Changed;
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

fn suggest_closest<'a>(target: &str, candidates: &[&'a str]) -> Option<&'a str> {
    if candidates.is_empty() {
        return None;
    }
    let lower_target = target.to_ascii_lowercase();

    if let Some(exact_prefix) = candidates.iter().copied().find(|candidate| {
        candidate.to_ascii_lowercase().starts_with(&lower_target)
            || lower_target.starts_with(&candidate.to_ascii_lowercase())
    }) {
        return Some(exact_prefix);
    }

    candidates
        .iter()
        .copied()
        .min_by_key(|candidate| levenshtein(&lower_target, &candidate.to_ascii_lowercase()))
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left_chars = left.chars().collect::<Vec<_>>();
    let right_chars = right.chars().collect::<Vec<_>>();
    if left_chars.is_empty() {
        return right_chars.len();
    }
    if right_chars.is_empty() {
        return left_chars.len();
    }

    let mut prev = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0; right_chars.len() + 1];
    for (i, l) in left_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, r) in right_chars.iter().enumerate() {
            let cost = usize::from(l != r);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[right_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::core_proxy_command_path;

    #[test]
    fn generated_proxy_command_mapping_resolves_known_entries() {
        assert_eq!(
            core_proxy_command_path("logs-path"),
            Some(&["logs", "path"] as &[&str])
        );
        assert_eq!(
            core_proxy_command_path("playbook-run"),
            Some(&["playbook", "run"] as &[&str])
        );
    }

    #[test]
    fn generated_proxy_command_mapping_ignores_non_proxy_commands() {
        assert!(core_proxy_command_path("list").is_none());
        assert!(core_proxy_command_path("doctor").is_none());
        assert!(core_proxy_command_path("does-not-exist").is_none());
    }
}

#[derive(Debug, Default)]
struct RebuildOptions {
    release: bool,
    json: bool,
    all_workspace_plugins: bool,
    mode: RebuildMode,
    selectors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
enum RebuildMode {
    #[default]
    AllWorkspace,
    Changed,
    List,
}

#[derive(Debug)]
struct BundledPlugin {
    plugin_id: String,
    short_name: String,
    crate_name: String,
}

#[derive(Debug, Serialize)]
struct RebuildSelectionReport {
    profile: String,
    mode: String,
    selected_targets: Vec<String>,
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

#[derive(Debug, Serialize)]
struct PluginDoctorReport {
    healthy: bool,
    enabled_plugins: Vec<String>,
    issues: Vec<String>,
}

bmux_plugin_sdk::export_plugin!(PluginCliPlugin, include_str!("../plugin.toml"));
