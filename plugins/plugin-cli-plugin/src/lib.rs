#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::{
    CoreCliCommandRequest, CoreCliCommandResponse, EXIT_OK, NativeCommandContext,
    PluginCliCommandRequest, PluginCliCommandResponse, PluginCommandError, RustPlugin,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
    let enabled_plugins = collect_enabled_plugins(context);
    let manifest_index = discover_manifest_index(context)?;
    let findings = collect_doctor_findings(context, &enabled_plugins, &manifest_index);
    let report = build_doctor_report(context, enabled_plugins.len(), findings);

    if as_json {
        let output = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed encoding plugin doctor json: {error}"))?;
        println!("{output}");
    } else {
        print_doctor_report(&report);
    }

    Ok(if report.healthy { EXIT_OK } else { 1 })
}

fn collect_enabled_plugins(
    context: &NativeCommandContext,
) -> Vec<&bmux_plugin_sdk::RegisteredPluginInfo> {
    let enabled_ids = context
        .enabled_plugins
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    context
        .registered_plugins
        .iter()
        .filter(|plugin| enabled_ids.contains(&plugin.id))
        .collect::<Vec<_>>()
}

fn collect_doctor_findings(
    context: &NativeCommandContext,
    enabled_plugins: &[&bmux_plugin_sdk::RegisteredPluginInfo],
    manifest_index: &BTreeMap<String, ManifestRecord>,
) -> Vec<DoctorFinding> {
    let mut findings = Vec::new();
    for plugin_id in &context.enabled_plugins {
        if !context
            .registered_plugins
            .iter()
            .any(|registered| registered.id == *plugin_id)
        {
            findings.push(DoctorFinding::error(
                "enabled_plugin_not_registered",
                format!("enabled plugin '{plugin_id}' was not found in the registry"),
                Some("Run 'bmux plugin list --json' and verify plugins.enabled in your config."),
            ));
        }
    }

    let available_capabilities = context
        .available_capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for plugin in enabled_plugins {
        for required in &plugin.required_capabilities {
            if !available_capabilities.contains(required) {
                findings.push(DoctorFinding::error(
                    "missing_required_capability",
                    format!(
                        "plugin '{}' requires unavailable capability '{}'",
                        plugin.id, required
                    ),
                    Some("Enable a plugin that provides the capability or disable the requiring plugin."),
                ));
            }
        }
        check_plugin_manifest_readiness(plugin, manifest_index, context, &mut findings);
    }

    findings
}

fn build_doctor_report(
    context: &NativeCommandContext,
    inspected_plugins: usize,
    findings: Vec<DoctorFinding>,
) -> PluginDoctorReport {
    let error_count = findings
        .iter()
        .filter(|finding| finding.severity == DoctorSeverity::Error)
        .count();
    let warning_count = findings
        .iter()
        .filter(|finding| finding.severity == DoctorSeverity::Warning)
        .count();
    let info_count = findings
        .iter()
        .filter(|finding| finding.severity == DoctorSeverity::Info)
        .count();

    let issues = findings
        .iter()
        .filter(|finding| finding.severity == DoctorSeverity::Error)
        .map(|finding| finding.message.clone())
        .collect::<Vec<_>>();
    let warnings = findings
        .iter()
        .filter(|finding| finding.severity == DoctorSeverity::Warning)
        .map(|finding| finding.message.clone())
        .collect::<Vec<_>>();

    PluginDoctorReport {
        healthy: error_count == 0,
        enabled_plugins: context.enabled_plugins.clone(),
        inspected_plugins,
        findings,
        error_count,
        warning_count,
        info_count,
        issues,
        warnings,
    }
}

fn print_doctor_report(report: &PluginDoctorReport) {
    println!(
        "plugin doctor: {} (inspected={} errors={} warnings={} info={})",
        if report.healthy { "ok" } else { "issues found" },
        report.inspected_plugins,
        report.error_count,
        report.warning_count,
        report.info_count
    );

    print_doctor_findings(report, DoctorSeverity::Error, "errors", true);
    print_doctor_findings(report, DoctorSeverity::Warning, "warnings", true);
    print_doctor_findings(report, DoctorSeverity::Info, "info", false);
}

fn print_doctor_findings(
    report: &PluginDoctorReport,
    severity: DoctorSeverity,
    header: &str,
    include_fix: bool,
) {
    let selected = report
        .findings
        .iter()
        .filter(|finding| finding.severity == severity)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return;
    }

    println!("{header}:");
    for finding in selected {
        println!("- [{}] {}", finding.code, finding.message);
        if include_fix && let Some(suggested_fix) = &finding.suggested_fix {
            println!("  fix: {suggested_fix}");
        }
    }
}

fn discover_manifest_index(
    context: &NativeCommandContext,
) -> Result<BTreeMap<String, ManifestRecord>, String> {
    let mut manifest_index = BTreeMap::new();
    for root in plugin_roots(context) {
        let Ok(report) = bmux_plugin::discover_plugin_manifests(&root) else {
            continue;
        };
        for manifest_path in report.manifest_paths {
            let manifest = bmux_plugin::PluginManifest::from_path(&manifest_path)
                .map_err(|error| format!("failed parsing {}: {error}", manifest_path.display()))?;
            manifest_index
                .entry(manifest.id.clone())
                .or_insert(ManifestRecord {
                    manifest,
                    manifest_path,
                });
        }
    }
    Ok(manifest_index)
}

fn check_plugin_manifest_readiness(
    plugin: &bmux_plugin_sdk::RegisteredPluginInfo,
    manifest_index: &BTreeMap<String, ManifestRecord>,
    context: &NativeCommandContext,
    findings: &mut Vec<DoctorFinding>,
) {
    let Some(record) = manifest_index.get(&plugin.id) else {
        findings.push(DoctorFinding::error(
            "manifest_missing",
            format!(
                "plugin '{}' is enabled but no manifest was discovered under configured plugin roots",
                plugin.id
            ),
            Some("Verify plugin_search_roots and plugin installation paths."),
        ));
        return;
    };

    let declaration = match record.manifest.to_declaration() {
        Ok(declaration) => declaration,
        Err(error) => {
            findings.push(DoctorFinding::error(
                "manifest_declaration_invalid",
                format!(
                    "plugin '{}' manifest failed declaration validation: {error}",
                    plugin.id
                ),
                Some("Run 'bmux plugin doctor --json' and fix plugin.toml declaration fields."),
            ));
            return;
        }
    };

    if !declaration
        .plugin_api
        .contains(context.host.plugin_api_version)
    {
        findings.push(DoctorFinding::error(
            "plugin_api_incompatible",
            format!(
                "plugin '{}' plugin_api range '{}' is incompatible with host API version {}",
                plugin.id, declaration.plugin_api, context.host.plugin_api_version
            ),
            Some("Upgrade/downgrade the plugin or host so plugin_api ranges overlap."),
        ));
    }
    if !declaration
        .native_abi
        .contains(context.host.plugin_abi_version)
    {
        findings.push(DoctorFinding::error(
            "native_abi_incompatible",
            format!(
                "plugin '{}' native_abi range '{}' is incompatible with host ABI version {}",
                plugin.id, declaration.native_abi, context.host.plugin_abi_version
            ),
            Some("Rebuild or reinstall plugin artifacts targeting this host ABI."),
        ));
    }

    check_plugin_runtime_readiness(plugin, record, findings);
}

fn check_plugin_runtime_readiness(
    plugin: &bmux_plugin_sdk::RegisteredPluginInfo,
    record: &ManifestRecord,
    findings: &mut Vec<DoctorFinding>,
) {
    match record.manifest.runtime {
        bmux_plugin::PluginRuntime::Native => {
            if plugin.bundled_static {
                return;
            }
            let Some(entry_path) = record.manifest.resolve_entry_path(
                record
                    .manifest_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            ) else {
                findings.push(DoctorFinding::error(
                    "native_entry_missing",
                    format!(
                        "plugin '{}' is missing an entry path for native runtime",
                        plugin.id
                    ),
                    Some("Set plugin.toml 'entry' to the plugin cdylib path."),
                ));
                return;
            };

            if !entry_path.exists() {
                findings.push(DoctorFinding::error(
                    "native_entry_not_found",
                    format!(
                        "plugin '{}' native entry does not exist: {}",
                        plugin.id,
                        entry_path.display()
                    ),
                    Some("Run 'bmux plugin rebuild --all-workspace-plugins' or fix plugin.toml entry."),
                ));
            }
        }
        bmux_plugin::PluginRuntime::Process => {
            let Some(command) = record
                .manifest
                .entry
                .as_ref()
                .and_then(|entry| entry.to_str())
            else {
                findings.push(DoctorFinding::error(
                    "process_entry_missing",
                    format!(
                        "plugin '{}' process runtime is missing entry command",
                        plugin.id
                    ),
                    Some("Set plugin.toml 'entry' to an executable command or path."),
                ));
                return;
            };

            match process_command_status(command, record) {
                ProcessCommandStatus::Available => findings.push(DoctorFinding::info(
                    "process_runtime_detected",
                    format!("plugin '{}' uses process runtime", plugin.id),
                )),
                ProcessCommandStatus::Missing(path) => findings.push(DoctorFinding::error(
                    "process_entry_not_found",
                    format!(
                        "plugin '{}' process command was not found: {}",
                        plugin.id,
                        path.display()
                    ),
                    Some("Ensure the command is on PATH or set plugin.toml entry to an absolute path."),
                )),
                ProcessCommandStatus::NotExecutable(path) => findings.push(DoctorFinding::error(
                    "process_entry_not_executable",
                    format!(
                        "plugin '{}' process command is not executable: {}",
                        plugin.id,
                        path.display()
                    ),
                    Some("Mark the process entry executable (for example: chmod +x <entry>)."),
                )),
            }
            findings.push(DoctorFinding::warning(
                "process_stdout_reserved",
                format!(
                    "plugin '{}' uses process runtime; ensure stdout emits only framed protocol responses",
                    plugin.id
                ),
                Some("Write diagnostics to stderr; keep stdout protocol-framed only."),
            ));
        }
    }
}

fn process_command_status(command: &str, record: &ManifestRecord) -> ProcessCommandStatus {
    let command_path = Path::new(command);
    if command_path.components().count() > 1 {
        let resolved = record.manifest.resolve_entry_path(
            record
                .manifest_path
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        );
        return match resolved {
            Some(path) if command_is_executable(&path) => ProcessCommandStatus::Available,
            Some(path) if path.exists() => ProcessCommandStatus::NotExecutable(path),
            Some(path) => ProcessCommandStatus::Missing(path),
            None => ProcessCommandStatus::Missing(PathBuf::from(command)),
        };
    }

    let mut first_non_exec: Option<PathBuf> = None;
    let available = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(command))
            .any(|candidate| {
                if command_is_executable(&candidate) {
                    return true;
                }
                if first_non_exec.is_none() && candidate.exists() {
                    first_non_exec = Some(candidate);
                }
                false
            })
    });

    if available {
        ProcessCommandStatus::Available
    } else if let Some(path) = first_non_exec {
        ProcessCommandStatus::NotExecutable(path)
    } else {
        ProcessCommandStatus::Missing(PathBuf::from(command))
    }
}

fn command_is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
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
        let suggestions = suggest_top_matches(&plugin_id, &available_ids, 3);
        return if suggestions.is_empty() {
            Err(format!(
                "plugin '{plugin_id}' was not found. Run 'bmux plugin list --json' to inspect available plugins."
            ))
        } else {
            Err(format!(
                "plugin '{plugin_id}' was not found. Did you mean: {}? Run 'bmux plugin list --json' to inspect available plugins.",
                suggestions.join(", ")
            ))
        };
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
        let suggestions = suggest_top_matches(&command_name, &known, 3);
        return if suggestions.is_empty() {
            Err(format!(
                "plugin '{plugin_id}' does not declare command '{command_name}'."
            ))
        } else {
            Err(format!(
                "plugin '{plugin_id}' does not declare command '{command_name}'. Did you mean: {}?",
                suggestions.join(", ")
            ))
        };
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
    let resolved_base_ref = resolve_changed_base_ref(&options)?;
    let metadata = cargo_metadata()?;
    let workspace_plugin_crates = workspace_plugin_cdylib_crates(&metadata);
    let bundled_plugins = discover_bundled_plugins(context)?;

    if matches!(options.mode, RebuildMode::List) {
        if options.json {
            let report = RebuildSelectionReport {
                profile: rebuild_profile_name(options.profile),
                base_ref: resolved_base_ref,
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

    let targets = select_rebuild_targets(
        &options,
        resolved_base_ref.as_deref(),
        &bundled_plugins,
        &workspace_plugin_crates,
    )?;

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
            profile: rebuild_profile_name(options.profile),
            base_ref: resolved_base_ref,
            selected_targets: targets.clone(),
            mode: rebuild_mode_name(&options),
        };
        let output = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed encoding rebuild json: {error}"))?;
        println!("{output}");
    }

    println!(
        "building plugin crates ({}): {}",
        if matches!(options.profile, BuildProfile::Release) {
            "release"
        } else {
            "debug"
        },
        targets.join(" ")
    );

    let mut command = ProcessCommand::new("cargo");
    command.arg("build");
    if matches!(options.profile, BuildProfile::Release) {
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

fn rebuild_profile_name(profile: BuildProfile) -> String {
    if matches!(profile, BuildProfile::Release) {
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
    base_ref: Option<&str>,
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
                changed_workspace_plugin_crates(base_ref, bundled_plugins, workspace_plugin_crates)?
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
    base_ref: Option<&str>,
    bundled: &[BundledPlugin],
    workspace_crates: &[String],
) -> Result<Vec<String>, String> {
    let changed_paths = collect_changed_paths(base_ref)?;
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

fn collect_changed_paths(base_ref: Option<&str>) -> Result<Vec<String>, String> {
    let mut changed = BTreeSet::new();

    let mut command_sets = Vec::new();
    if let Some(base_ref) = base_ref {
        command_sets.push(vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "--relative".to_string(),
            format!("{base_ref}...HEAD"),
        ]);
    } else {
        command_sets.push(vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "--relative".to_string(),
            "HEAD".to_string(),
        ]);
    }
    command_sets.push(vec![
        "diff".to_string(),
        "--name-only".to_string(),
        "--relative".to_string(),
        "--cached".to_string(),
    ]);
    command_sets.push(vec![
        "ls-files".to_string(),
        "--others".to_string(),
        "--exclude-standard".to_string(),
    ]);

    for args in command_sets {
        let output = ProcessCommand::new("git")
            .args(&args)
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

fn resolve_changed_base_ref(options: &RebuildOptions) -> Result<Option<String>, String> {
    if let Some(base_ref) = &options.base_ref {
        return Ok(Some(base_ref.clone()));
    }
    if !options.against_master {
        return Ok(None);
    }

    if git_ref_exists("origin/master")? {
        return Ok(Some("origin/master".to_string()));
    }

    Err("--against-master requested but origin/master does not exist locally".to_string())
}

fn git_ref_exists(reference: &str) -> Result<bool, String> {
    let output = ProcessCommand::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .map_err(|error| format!("failed executing git rev-parse for '{reference}': {error}"))?;
    Ok(output.status.success())
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

    let mut index = 0_usize;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--" {
            positional_mode = true;
            index += 1;
            continue;
        }

        if !positional_mode {
            match argument.as_str() {
                "--release" => {
                    options.profile = BuildProfile::Release;
                    index += 1;
                    continue;
                }
                "--list" => {
                    options.mode = RebuildMode::List;
                    index += 1;
                    continue;
                }
                "--json" => {
                    options.json = true;
                    index += 1;
                    continue;
                }
                "--changed" => {
                    options.mode = RebuildMode::Changed;
                    index += 1;
                    continue;
                }
                "--against-master" => {
                    options.against_master = true;
                    index += 1;
                    continue;
                }
                "--all-workspace-plugins" => {
                    options.all_workspace_plugins = true;
                    index += 1;
                    continue;
                }
                "--base" => {
                    let Some(base_ref) = arguments.get(index + 1) else {
                        return Err("--base requires a git ref argument".to_string());
                    };
                    options.base_ref = Some(base_ref.clone());
                    index += 2;
                    continue;
                }
                value if value.starts_with("--base=") => {
                    let base_ref = value.trim_start_matches("--base=").trim();
                    if base_ref.is_empty() {
                        return Err("--base requires a non-empty git ref".to_string());
                    }
                    options.base_ref = Some(base_ref.to_string());
                    index += 1;
                    continue;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown option: {value}"));
                }
                _ => {}
            }
        }

        options.selectors.push(argument.clone());
        index += 1;
    }

    if options.base_ref.is_some() && options.against_master {
        return Err("--base and --against-master cannot be used together".to_string());
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

fn suggest_top_matches(target: &str, candidates: &[&str], limit: usize) -> Vec<String> {
    if candidates.is_empty() || limit == 0 {
        return Vec::new();
    }

    let lower_target = target.to_ascii_lowercase();
    let max_distance = lower_target.chars().count().max(3) / 2 + 1;

    let mut ranked = candidates
        .iter()
        .map(|candidate| {
            let lower_candidate = candidate.to_ascii_lowercase();
            let distance = levenshtein(&lower_target, &lower_candidate);
            let prefix_match = lower_candidate.starts_with(&lower_target)
                || lower_target.starts_with(&lower_candidate);
            (distance, !prefix_match, *candidate)
        })
        .filter(|(distance, prefix_penalty, _)| *distance <= max_distance || !*prefix_penalty)
        .collect::<Vec<_>>();

    ranked.sort_unstable();
    ranked
        .into_iter()
        .map(|(_, _, candidate)| candidate.to_string())
        .take(limit)
        .collect()
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
    use super::{RebuildMode, core_proxy_command_path, parse_rebuild_options, suggest_top_matches};

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

    #[test]
    fn suggest_top_matches_limits_and_filters_results() {
        let candidates = vec!["bmux.plugin_cli", "bmux.permissions", "bmux.windows"];
        let matches = suggest_top_matches("bmux.plugin", &candidates, 2);
        assert!(!matches.is_empty());
        assert_eq!(matches[0], "bmux.plugin_cli");
    }

    #[test]
    fn parse_rebuild_options_supports_base_and_against_master() {
        let options = parse_rebuild_options(&[
            "--changed".to_string(),
            "--base".to_string(),
            "origin/master".to_string(),
        ])
        .expect("options should parse");
        assert!(matches!(options.mode, RebuildMode::Changed));
        assert_eq!(options.base_ref.as_deref(), Some("origin/master"));
        assert!(!options.against_master);

        let against_master =
            parse_rebuild_options(&["--changed".to_string(), "--against-master".to_string()])
                .expect("against-master should parse");
        assert!(against_master.against_master);
    }

    #[test]
    fn parse_rebuild_options_rejects_conflicting_base_modes() {
        let error = parse_rebuild_options(&[
            "--against-master".to_string(),
            "--base=origin/master".to_string(),
        ])
        .expect_err("conflicting modes should error");
        assert!(error.contains("cannot be used together"));
    }
}

#[derive(Debug, Default)]
struct RebuildOptions {
    profile: BuildProfile,
    json: bool,
    all_workspace_plugins: bool,
    mode: RebuildMode,
    base_ref: Option<String>,
    against_master: bool,
    selectors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
enum BuildProfile {
    #[default]
    Debug,
    Release,
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

#[derive(Debug)]
struct ManifestRecord {
    manifest: bmux_plugin::PluginManifest,
    manifest_path: PathBuf,
}

#[derive(Debug)]
enum ProcessCommandStatus {
    Available,
    Missing(PathBuf),
    NotExecutable(PathBuf),
}

#[derive(Debug, Serialize)]
struct RebuildSelectionReport {
    profile: String,
    mode: String,
    base_ref: Option<String>,
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
    inspected_plugins: usize,
    findings: Vec<DoctorFinding>,
    error_count: usize,
    warning_count: usize,
    info_count: usize,
    issues: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DoctorSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Serialize)]
struct DoctorFinding {
    code: String,
    severity: DoctorSeverity,
    message: String,
    suggested_fix: Option<String>,
}

impl DoctorFinding {
    fn error(code: &str, message: String, suggested_fix: Option<&str>) -> Self {
        Self {
            code: code.to_string(),
            severity: DoctorSeverity::Error,
            message,
            suggested_fix: suggested_fix.map(str::to_string),
        }
    }

    fn warning(code: &str, message: String, suggested_fix: Option<&str>) -> Self {
        Self {
            code: code.to_string(),
            severity: DoctorSeverity::Warning,
            message,
            suggested_fix: suggested_fix.map(str::to_string),
        }
    }

    fn info(code: &str, message: String) -> Self {
        Self {
            code: code.to_string(),
            severity: DoctorSeverity::Info,
            message,
            suggested_fix: None,
        }
    }
}

bmux_plugin_sdk::export_plugin!(PluginCliPlugin, include_str!("../plugin.toml"));
