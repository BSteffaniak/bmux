use crate::{
    DoctorFinding, DoctorSeverity, ManifestRecord, PluginDoctorReport, ProcessCommandStatus,
    has_flag, plugin_roots,
};
use bmux_plugin_sdk::{EXIT_OK, NativeCommandContext, RegisteredPluginInfo};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn run_doctor_command(context: &NativeCommandContext) -> Result<i32, String> {
    let as_json = has_flag(&context.arguments, "json");
    let strict = has_flag(&context.arguments, "strict");
    let summary_only = has_flag(&context.arguments, "summary-only");
    let severity_filter = parse_option_value(&context.arguments, "severity")?;
    let code_filter = parse_option_value(&context.arguments, "code")?;
    let enabled_plugins = collect_enabled_plugins(context);
    let manifest_index = discover_manifest_index(context)?;
    let findings = collect_doctor_findings(context, &enabled_plugins, &manifest_index);
    let filtered_findings =
        filter_doctor_findings(findings, severity_filter.as_deref(), code_filter.as_deref())?;
    let mut report = build_doctor_report(context, enabled_plugins.len(), strict, filtered_findings);

    if summary_only {
        report.findings.clear();
    }

    if as_json {
        let output = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed encoding plugin doctor json: {error}"))?;
        println!("{output}");
    } else if summary_only {
        print_doctor_summary(&report);
    } else {
        print_doctor_report(&report);
    }

    Ok(if report.healthy { EXIT_OK } else { 1 })
}

fn parse_option_value(arguments: &[String], option_name: &str) -> Result<Option<String>, String> {
    let long = format!("--{option_name}");
    let long_eq = format!("--{option_name}=");

    let mut value = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == &long {
            let Some(next) = arguments.get(index + 1) else {
                return Err(format!("{long} requires a value"));
            };
            if next.starts_with('-') {
                return Err(format!("{long} requires a value"));
            }
            value = Some(next.clone());
            index += 2;
            continue;
        }
        if let Some(inline) = argument.strip_prefix(&long_eq) {
            if inline.is_empty() {
                return Err(format!("{long} requires a value"));
            }
            value = Some(inline.to_string());
        }
        index += 1;
    }

    Ok(value)
}

fn filter_doctor_findings(
    findings: Vec<DoctorFinding>,
    severity_filter: Option<&str>,
    code_filter: Option<&str>,
) -> Result<Vec<DoctorFinding>, String> {
    let severity_filter = match severity_filter {
        Some("error") => Some(DoctorSeverity::Error),
        Some("warning") => Some(DoctorSeverity::Warning),
        Some("info") => Some(DoctorSeverity::Info),
        Some(other) => {
            return Err(format!(
                "--severity must be one of: error, warning, info (received '{other}')"
            ));
        }
        None => None,
    };

    Ok(findings
        .into_iter()
        .filter(|finding| severity_filter.is_none_or(|severity| finding.severity == severity))
        .filter(|finding| code_filter.is_none_or(|code| finding.code.contains(code)))
        .collect())
}

fn collect_enabled_plugins(context: &NativeCommandContext) -> Vec<&RegisteredPluginInfo> {
    let enabled_ids = context
        .enabled_plugins
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    context
        .registered_plugins
        .iter()
        .filter(|plugin| enabled_ids.contains(plugin.id.as_str()))
        .collect::<Vec<_>>()
}

fn collect_doctor_findings(
    context: &NativeCommandContext,
    enabled_plugins: &[&RegisteredPluginInfo],
    manifest_index: &BTreeMap<String, ManifestRecord>,
) -> Vec<DoctorFinding> {
    let mut findings = Vec::new();
    let registered_ids = context
        .registered_plugins
        .iter()
        .map(|plugin| plugin.id.as_str())
        .collect::<BTreeSet<_>>();
    for plugin_id in &context.enabled_plugins {
        if !registered_ids.contains(plugin_id.as_str()) {
            findings.push(DoctorFinding::error(
                "enabled_plugin_not_registered",
                format!("enabled plugin '{plugin_id}' was not found in the registry"),
                Some("Run 'bmux plugin list --json' and update plugins.enabled in your config."),
            ));
        }
    }

    let available_capabilities = context
        .available_capabilities
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for plugin in enabled_plugins {
        for required in &plugin.required_capabilities {
            if !available_capabilities.contains(required.as_str()) {
                findings.push(DoctorFinding::error(
                    "missing_required_capability",
                    format!(
                        "plugin '{}' requires unavailable capability '{}'",
                        plugin.id, required
                    ),
                    Some(
                        "Run 'bmux plugin list --json' to inspect provided capabilities, then adjust plugins.enabled.",
                    ),
                ));
            }
        }
        check_plugin_manifest_readiness(plugin, manifest_index, context, &mut findings);
    }

    check_manifest_cli_path_conflicts(enabled_plugins, manifest_index, &mut findings);

    findings
}

fn build_doctor_report(
    context: &NativeCommandContext,
    inspected_plugins: usize,
    strict_mode: bool,
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

    let healthy = error_count == 0 && (!strict_mode || warning_count == 0);

    PluginDoctorReport {
        healthy,
        strict_mode,
        enabled_plugins: context.enabled_plugins.clone(),
        inspected_plugins,
        findings,
        error_count,
        warning_count,
        info_count,
        issues,
        warnings,
        next_steps: doctor_next_steps(healthy, strict_mode, error_count, warning_count),
    }
}

fn check_manifest_cli_path_conflicts(
    enabled_plugins: &[&RegisteredPluginInfo],
    manifest_index: &BTreeMap<String, ManifestRecord>,
    findings: &mut Vec<DoctorFinding>,
) {
    let mut owners: BTreeMap<Vec<String>, Vec<String>> = BTreeMap::new();

    for plugin in enabled_plugins {
        let Some(record) = manifest_index.get(&plugin.id) else {
            continue;
        };
        for command in &record.manifest.commands {
            if !command.expose_in_cli {
                continue;
            }
            for path in command.cli_paths() {
                owners.entry(path).or_default().push(plugin.id.clone());
            }
        }
    }

    for (path, mut path_owners) in owners {
        path_owners.sort();
        path_owners.dedup();
        if path_owners.len() <= 1 {
            continue;
        }
        let path_label = path.join(" ");
        findings.push(DoctorFinding::warning(
            "cli_path_multi_owner",
            format!(
                "CLI path '{}' is declared by multiple enabled plugins: {}",
                path_label,
                path_owners.join(", ")
            ),
            Some(
                "Run 'bmux plugin list --json' and remove overlapping CLI paths or disable one owner plugin.",
            ),
        ));
    }
}

fn print_doctor_report(report: &PluginDoctorReport) {
    println!(
        "plugin doctor: {} (inspected={} errors={} warnings={} info={} strict={})",
        if report.healthy { "ok" } else { "issues found" },
        report.inspected_plugins,
        report.error_count,
        report.warning_count,
        report.info_count,
        report.strict_mode
    );

    print_doctor_findings(report, DoctorSeverity::Error, "errors", true);
    print_doctor_findings(report, DoctorSeverity::Warning, "warnings", true);
    print_doctor_findings(report, DoctorSeverity::Info, "info", false);

    if !report.healthy {
        for note in doctor_failure_notes(report) {
            println!("{note}");
        }
    }
}

fn print_doctor_summary(report: &PluginDoctorReport) {
    println!(
        "plugin doctor summary: {} (inspected={} errors={} warnings={} info={} strict={})",
        if report.healthy { "ok" } else { "issues found" },
        report.inspected_plugins,
        report.error_count,
        report.warning_count,
        report.info_count,
        report.strict_mode
    );
    if !report.healthy {
        for note in doctor_failure_notes(report) {
            println!("{note}");
        }
    }
}

fn doctor_failure_notes(report: &PluginDoctorReport) -> Vec<String> {
    let mut notes = Vec::new();
    if report.strict_mode && report.error_count == 0 && report.warning_count > 0 {
        notes.push(
            "strict mode failed because warnings are treated as failures. Fix warnings above or run without --strict.".to_string(),
        );
    }
    notes.push(
        "Next: run 'bmux plugin doctor --json --strict' for machine-readable diagnostics"
            .to_string(),
    );
    notes
}

fn doctor_next_steps(
    healthy: bool,
    strict_mode: bool,
    error_count: usize,
    warning_count: usize,
) -> Vec<String> {
    if healthy {
        return Vec::new();
    }

    let mut steps = Vec::new();
    if strict_mode && error_count == 0 && warning_count > 0 {
        steps.push(
            "strict mode failed because warnings are treated as failures. Fix warnings above or run without --strict.".to_string(),
        );
    }
    steps.push(
        "Next: run 'bmux plugin doctor --json --strict' for machine-readable diagnostics"
            .to_string(),
    );
    steps
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
    plugin: &RegisteredPluginInfo,
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
            Some(
                "Verify plugin_search_roots and plugin installation paths, then run 'bmux plugin doctor --json --strict'.",
            ),
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
                Some("Fix plugin.toml declaration fields, then run 'bmux plugin doctor --json --strict'."),
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
            Some(
                "Upgrade or downgrade the plugin/host so plugin_api ranges overlap, then rerun 'bmux plugin doctor --json --strict'.",
            ),
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
            Some(
                "Run 'bmux plugin rebuild --all-workspace-plugins' or reinstall artifacts targeting this host ABI.",
            ),
        ));
    }

    check_plugin_runtime_readiness(plugin, record, findings);
}

fn check_plugin_runtime_readiness(
    plugin: &RegisteredPluginInfo,
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
                    Some(
                        "Set plugin.toml 'entry' to the plugin cdylib path, then run 'bmux plugin doctor --json --strict'.",
                    ),
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
                    Some(
                        "Set plugin.toml 'entry' to an executable command or path, then run 'bmux plugin doctor --json --strict'.",
                    ),
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
                    Some(
                        "Ensure the command is on PATH or set plugin.toml entry to an absolute path, then rerun 'bmux plugin doctor --json --strict'.",
                    ),
                )),
                ProcessCommandStatus::NotExecutable(path) => findings.push(DoctorFinding::error(
                    "process_entry_not_executable",
                    format!(
                        "plugin '{}' process command is not executable: {}",
                        plugin.id,
                        path.display()
                    ),
                    Some(
                        "Mark the process entry executable (for example: chmod +x <entry>), then rerun 'bmux plugin doctor --json --strict'.",
                    ),
                )),
            }
            findings.push(DoctorFinding::warning(
                "process_stdout_reserved",
                format!(
                    "plugin '{}' uses process runtime; ensure stdout emits only framed protocol responses",
                    plugin.id
                ),
                Some(
                    "Write diagnostics to stderr and keep stdout protocol-framed only; validate with 'bmux plugin doctor --json --strict'.",
                ),
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

#[cfg(test)]
mod tests {
    use super::{
        doctor_failure_notes, doctor_next_steps, filter_doctor_findings, parse_option_value,
    };
    use crate::{DoctorFinding, DoctorSeverity, PluginDoctorReport};

    fn test_doctor_report(
        strict_mode: bool,
        error_count: usize,
        warning_count: usize,
    ) -> PluginDoctorReport {
        PluginDoctorReport {
            healthy: error_count == 0 && (!strict_mode || warning_count == 0),
            strict_mode,
            enabled_plugins: Vec::new(),
            inspected_plugins: 0,
            findings: vec![DoctorFinding {
                code: "test".to_string(),
                severity: DoctorSeverity::Warning,
                message: "test warning".to_string(),
                suggested_fix: Some("test fix".to_string()),
            }],
            error_count,
            warning_count,
            info_count: 0,
            issues: Vec::new(),
            warnings: Vec::new(),
            next_steps: Vec::new(),
        }
    }

    #[test]
    fn doctor_failure_notes_includes_strict_warning_note_when_applicable() {
        let report = test_doctor_report(true, 0, 1);
        let notes = doctor_failure_notes(&report);
        assert!(
            notes
                .iter()
                .any(|note| note.contains("warnings are treated as failures"))
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("bmux plugin doctor --json --strict"))
        );
        assert!(notes.iter().any(|note| note.starts_with("Next:")));
    }

    #[test]
    fn doctor_failure_notes_omits_strict_warning_note_when_errors_exist() {
        let report = test_doctor_report(true, 1, 1);
        let notes = doctor_failure_notes(&report);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("bmux plugin doctor --json --strict"));
        assert!(notes[0].starts_with("Next:"));
    }

    #[test]
    fn doctor_next_steps_matches_failure_shape() {
        let strict_warning_only = doctor_next_steps(false, true, 0, 1);
        assert_eq!(strict_warning_only.len(), 2);
        assert!(strict_warning_only[0].contains("warnings are treated as failures"));
        assert_eq!(
            strict_warning_only[1],
            "Next: run 'bmux plugin doctor --json --strict' for machine-readable diagnostics"
        );

        let hard_errors = doctor_next_steps(false, true, 1, 0);
        assert_eq!(hard_errors.len(), 1);
        assert_eq!(
            hard_errors[0],
            "Next: run 'bmux plugin doctor --json --strict' for machine-readable diagnostics"
        );

        let healthy = doctor_next_steps(true, false, 0, 0);
        assert!(healthy.is_empty());
    }

    #[test]
    fn parse_option_value_supports_inline_and_separate_forms() {
        let inline =
            parse_option_value(&["--severity=warning".to_string()], "severity").expect("inline");
        assert_eq!(inline.as_deref(), Some("warning"));

        let separate = parse_option_value(&["--code".to_string(), "manifest".to_string()], "code")
            .expect("separate");
        assert_eq!(separate.as_deref(), Some("manifest"));
    }

    #[test]
    fn filter_doctor_findings_filters_by_severity_and_code() {
        let findings_for_severity = vec![
            DoctorFinding {
                code: "manifest_missing".to_string(),
                severity: DoctorSeverity::Error,
                message: "error".to_string(),
                suggested_fix: None,
            },
            DoctorFinding {
                code: "process_stdout_reserved".to_string(),
                severity: DoctorSeverity::Warning,
                message: "warning".to_string(),
                suggested_fix: None,
            },
        ];
        let findings_for_code = vec![
            DoctorFinding {
                code: "manifest_missing".to_string(),
                severity: DoctorSeverity::Error,
                message: "error".to_string(),
                suggested_fix: None,
            },
            DoctorFinding {
                code: "process_stdout_reserved".to_string(),
                severity: DoctorSeverity::Warning,
                message: "warning".to_string(),
                suggested_fix: None,
            },
        ];

        let severity_only = filter_doctor_findings(findings_for_severity, Some("warning"), None)
            .expect("severity filter should pass");
        assert_eq!(severity_only.len(), 1);
        assert_eq!(severity_only[0].code, "process_stdout_reserved");

        let code_only = filter_doctor_findings(findings_for_code, None, Some("manifest"))
            .expect("code filter should pass");
        assert_eq!(code_only.len(), 1);
        assert_eq!(code_only[0].code, "manifest_missing");
    }
}
