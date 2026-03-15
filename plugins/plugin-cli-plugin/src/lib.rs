#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{
    CapabilityProvider, CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor,
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginManifest,
    PluginRegistry, RustPlugin, discover_plugin_manifests_in_roots, load_registered_plugin,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

#[derive(Default)]
struct PluginCliPlugin;

impl RustPlugin for PluginCliPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor::builder("bmux.plugin_cli", "bmux Plugin CLI")
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .description("Shipped bmux plugin subcommands")
            .require_capability("bmux.commands")
            .expect("capability should parse")
            .command(
                PluginCommand::new("list", "List discovered plugins")
                    .path(["plugin", "list"])
                    .argument(PluginCommandArgument::flag("json").short('j'))
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .command(
                PluginCommand::new("run", "Run a declared plugin command")
                    .path(["plugin", "run"])
                    .argument(
                        PluginCommandArgument::positional(
                            "plugin",
                            PluginCommandArgumentKind::String,
                        )
                        .required(true)
                        .value_name("PLUGIN"),
                    )
                    .argument(
                        PluginCommandArgument::positional(
                            "command",
                            PluginCommandArgumentKind::String,
                        )
                        .position(1)
                        .required(true)
                        .value_name("COMMAND"),
                    )
                    .argument(
                        PluginCommandArgument::positional(
                            "args",
                            PluginCommandArgumentKind::String,
                        )
                        .position(2)
                        .multiple(true)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true)
                        .value_name("ARGS"),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .command(
                PluginCommand::new("rebuild", "Rebuild bundled plugin crates")
                    .path(["plugin", "rebuild"])
                    .argument(PluginCommandArgument::flag("release"))
                    .argument(PluginCommandArgument::flag("list"))
                    .argument(PluginCommandArgument::flag("all-workspace-plugins"))
                    .argument(
                        PluginCommandArgument::positional(
                            "selectors",
                            PluginCommandArgumentKind::String,
                        )
                        .multiple(true)
                        .allow_hyphen_values(true)
                        .value_name("SELECTOR"),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .build()
            .expect("descriptor should validate")
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        let result = match context.command.as_str() {
            "list" => run_list_command(&context),
            "run" => run_run_command(&context),
            "rebuild" => run_rebuild_command(&context),
            _ => Err(format!("unsupported command '{}'", context.command)),
        };

        match result {
            Ok(status) => status,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        }
    }
}

fn run_list_command(context: &NativeCommandContext) -> Result<i32, String> {
    let as_json = has_flag(&context.arguments, "json");
    let registry = scan_plugins_with_bundled_entry_fallback(context)?;
    let enabled = context.enabled_plugins.iter().collect::<BTreeSet<_>>();

    let mut entries = registry
        .iter()
        .map(|plugin| PluginListEntry {
            id: plugin.declaration.id.as_str().to_string(),
            display_name: plugin.declaration.display_name.clone(),
            version: plugin.declaration.plugin_version.clone(),
            enabled: enabled.contains(&plugin.declaration.id.as_str().to_string()),
            required_capabilities: plugin
                .declaration
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            provided_capabilities: plugin
                .declaration
                .provided_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            commands: plugin
                .declaration
                .commands
                .iter()
                .map(|command| command.name.clone())
                .collect(),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.id.cmp(&right.id));

    if as_json {
        let output = serde_json::to_string_pretty(&entries)
            .map_err(|error| format!("failed encoding plugin list json: {error}"))?;
        println!("{output}");
        return Ok(0);
    }

    if entries.is_empty() {
        println!("no plugins discovered");
        return Ok(0);
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

    Ok(0)
}

fn run_run_command(context: &NativeCommandContext) -> Result<i32, String> {
    if context.arguments.len() < 2 {
        return Err("usage: bmux plugin run <plugin> <command> [args ...]".to_string());
    }

    let plugin_id = &context.arguments[0];
    let command_name = &context.arguments[1];
    let args = context.arguments[2..].to_vec();

    if plugin_id == &context.plugin_id {
        return Err(
            "running 'bmux.plugin_cli' via 'bmux plugin run' is not supported (self-invocation deadlock guard)"
                .to_string(),
        );
    }

    let registry = scan_plugins_with_bundled_entry_fallback(context)?;
    let available = registry.plugin_ids();
    let plugin = registry
        .get(plugin_id)
        .ok_or_else(|| format_plugin_not_found_message(plugin_id, &available))?;

    if !context
        .enabled_plugins
        .iter()
        .any(|enabled| enabled == plugin_id)
    {
        return Err(format_plugin_not_enabled_message(plugin_id));
    }

    let available_capabilities = context
        .available_capabilities
        .iter()
        .filter_map(|scope| HostScope::new(scope).ok())
        .map(|capability| {
            let provider = CapabilityProvider {
                capability: capability.clone(),
                provider: bmux_plugin::ProviderId::Host,
            };
            (capability, provider)
        })
        .collect::<BTreeMap<_, _>>();

    let loaded = load_registered_plugin(plugin, &context.host, &available_capabilities)
        .map_err(|error| format!("failed loading enabled plugin '{plugin_id}': {error}"))?;

    let command_context = NativeCommandContext {
        plugin_id: plugin.declaration.id.as_str().to_string(),
        command: command_name.clone(),
        arguments: args.clone(),
        required_capabilities: plugin
            .declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: plugin
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: context.services.clone(),
        available_capabilities: context.available_capabilities.clone(),
        enabled_plugins: context.enabled_plugins.clone(),
        plugin_search_roots: context.plugin_search_roots.clone(),
        host: context.host.clone(),
        connection: context.connection.clone(),
        settings: context.plugin_settings_map.get(plugin_id).cloned(),
        plugin_settings_map: context.plugin_settings_map.clone(),
        host_kernel_bridge: context.host_kernel_bridge,
    };

    let (status, _outcome) = loaded
        .run_command_with_context_and_outcome(command_name, &args, Some(&command_context))
        .map_err(|error| format_plugin_command_run_error(plugin_id, command_name, &error))?;

    Ok(status)
}

fn run_rebuild_command(context: &NativeCommandContext) -> Result<i32, String> {
    let options = parse_rebuild_options(&context.arguments)?;
    let metadata = cargo_metadata()?;
    let workspace_plugin_crates = workspace_plugin_cdylib_crates(&metadata);
    let bundled_plugins = discover_bundled_plugins(context)?;

    if options.list_only {
        print_discovered_plugins(&bundled_plugins, &workspace_plugin_crates);
        return Ok(0);
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

    Ok(0)
}

fn discover_bundled_plugins(context: &NativeCommandContext) -> Result<Vec<BundledPlugin>, String> {
    let mut discovered = Vec::new();
    let mut seen_ids = BTreeSet::new();

    for root in plugin_roots(context) {
        let Ok(report) = bmux_plugin::discover_plugin_manifests(&root) else {
            continue;
        };
        for manifest_path in report.manifest_paths {
            let is_bundled = manifest_path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .is_some_and(|segment| segment == "bundled");
            if !is_bundled {
                continue;
            }

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
                        "invalid bundled plugin path for manifest {}",
                        manifest_path.display()
                    )
                })?
                .to_string();
            let crate_name = entry_to_crate_name(&manifest.entry).ok_or_else(|| {
                format!(
                    "invalid bundled plugin entry '{}' in {}",
                    manifest.entry.display(),
                    manifest_path.display()
                )
            })?;

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

fn scan_plugins_with_bundled_entry_fallback(
    context: &NativeCommandContext,
) -> Result<PluginRegistry, String> {
    let roots = plugin_roots(context);
    let reports = discover_plugin_manifests_in_roots(&roots).map_err(|error| error.to_string())?;
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let mut registry = PluginRegistry::new();

    for report in reports {
        for manifest_path in report.manifest_paths {
            let mut manifest = PluginManifest::from_path(&manifest_path)
                .map_err(|error| format!("failed parsing {}: {error}", manifest_path.display()))?;

            let entry_exists = manifest
                .resolve_entry_path(manifest_path.parent().unwrap_or_else(|| Path::new(".")))
                .exists();
            if !entry_exists {
                if let Some(executable_dir) = executable_dir.as_ref() {
                    let candidate = executable_dir.join(&manifest.entry);
                    if candidate.exists() {
                        manifest.entry = candidate;
                    }
                }
            }

            registry
                .register_manifest_from_root(&report.search_root, &manifest_path, manifest)
                .map_err(|error| {
                    format!(
                        "failed registering plugin manifest {}: {error}",
                        manifest_path.display()
                    )
                })?;
        }
    }

    Ok(registry)
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

fn has_flag(arguments: &[String], long_name: &str) -> bool {
    let long_flag = format!("--{long_name}");
    arguments.iter().any(|argument| argument == &long_flag)
}

fn format_plugin_not_found_message<S: AsRef<str>>(plugin_id: &str, available: &[S]) -> String {
    if available.is_empty() {
        format!("plugin '{plugin_id}' was not found")
    } else {
        let available = available
            .iter()
            .map(|entry| entry.as_ref())
            .collect::<Vec<_>>();
        format!(
            "plugin '{plugin_id}' was not found (available: {})",
            available.join(", ")
        )
    }
}

fn format_plugin_not_enabled_message(plugin_id: &str) -> String {
    format!(
        "plugin '{plugin_id}' is not enabled; remove it from plugins.disabled or add it under plugins.enabled to run commands"
    )
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

bmux_plugin::export_plugin!(PluginCliPlugin);
