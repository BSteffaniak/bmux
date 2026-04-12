use crate::{
    BaseSelector, BuildProfile, BundledPlugin, DiffRangeMode, ExecutionMode, OutputMode,
    RebuildMode, RebuildOptions, RebuildSelectionReport, RebuildTargetSelection, WorkspaceFlag,
    dir_name_to_crate_name, entry_to_crate_name, plugin_roots,
};
use bmux_plugin_sdk::{EXIT_OK, NativeCommandContext};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command as ProcessCommand;

pub fn run_rebuild_command(context: &NativeCommandContext) -> Result<i32, String> {
    let options = parse_rebuild_options(&context.arguments)?;
    let resolved_base_ref = resolve_changed_base_ref(&options)?;
    let metadata = cargo_metadata()?;
    let workspace_plugin_crates = workspace_plugin_cdylib_crates(&metadata);
    let bundled_plugins = discover_bundled_plugins(context)?;

    if matches!(options.mode, RebuildMode::List) {
        emit_rebuild_selection_report(&options, resolved_base_ref, Vec::new())?;
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

    validate_selected_targets(&targets, &workspace_plugin_crates)?;

    emit_rebuild_selection_report(&options, resolved_base_ref, targets.clone())?;

    if matches!(options.execution_mode, ExecutionMode::DryRun) {
        println!("dry-run enabled; skipping cargo build execution");
        return Ok(EXIT_OK);
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

    execute_rebuild_build(options.profile, &targets)?;

    Ok(EXIT_OK)
}

fn emit_rebuild_selection_report(
    options: &RebuildOptions,
    base_ref: Option<String>,
    targets: Vec<String>,
) -> Result<(), String> {
    let selected_by = build_rebuild_target_selection(options, &targets);
    let report = RebuildSelectionReport {
        profile: rebuild_profile_name(options.profile),
        base_ref,
        selected_targets: targets,
        selected_by,
        mode: rebuild_mode_name(options),
    };
    if matches!(options.output_mode, OutputMode::Json) {
        let output = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed encoding rebuild json: {error}"))?;
        println!("{output}");
    } else {
        print_rebuild_selection_text(&report);
    }
    Ok(())
}

pub fn build_rebuild_target_selection(
    options: &RebuildOptions,
    targets: &[String],
) -> Vec<RebuildTargetSelection> {
    targets
        .iter()
        .map(|crate_name| {
            let reason = if !options.selectors.is_empty() {
                "selector"
            } else if matches!(options.mode, RebuildMode::Changed) {
                "changed"
            } else {
                "workspace-default"
            };
            RebuildTargetSelection {
                crate_name: crate_name.clone(),
                reason: reason.to_string(),
            }
        })
        .collect::<Vec<_>>()
}

fn print_rebuild_selection_text(report: &RebuildSelectionReport) {
    println!(
        "rebuild selection: mode={} profile={} targets={}",
        report.mode,
        report.profile,
        report.selected_targets.len()
    );
    if let Some(base_ref) = &report.base_ref {
        println!("base ref: {base_ref}");
    }
    if report.selected_by.is_empty() {
        println!("selected by: (none)");
        return;
    }
    println!("selected by:");
    for entry in &report.selected_by {
        println!("- {} ({})", entry.crate_name, entry.reason);
    }
}

fn validate_selected_targets(
    targets: &[String],
    workspace_plugin_crates: &[String],
) -> Result<(), String> {
    for crate_name in targets {
        if !workspace_plugin_crates
            .iter()
            .any(|known| known == crate_name)
        {
            return Err(format!(
                "selected crate '{crate_name}' is not a workspace plugin cdylib crate"
            ));
        }
    }
    Ok(())
}

fn execute_rebuild_build(profile: BuildProfile, targets: &[String]) -> Result<(), String> {
    let mut command = ProcessCommand::new("cargo");
    command.arg("build");
    if matches!(profile, BuildProfile::Release) {
        command.arg("--release");
    }
    for crate_name in targets {
        command.arg("-p");
        command.arg(crate_name);
    }

    let status = command
        .status()
        .map_err(|error| format!("failed executing cargo build: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("cargo build failed for selected plugin crates".to_string())
    }
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
            for crate_name in changed_workspace_plugin_crates(
                base_ref,
                options.diff_range_mode,
                bundled_plugins,
                workspace_plugin_crates,
            )? {
                add_target(&crate_name);
            }
        }
        RebuildMode::AllWorkspace | RebuildMode::List => {
            if matches!(options.workspace_flag, WorkspaceFlag::ExplicitAll) {
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
    diff_range_mode: DiffRangeMode,
    bundled: &[BundledPlugin],
    workspace_crates: &[String],
) -> Result<Vec<String>, String> {
    let changed_paths = collect_changed_paths(base_ref, diff_range_mode)?;
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

fn collect_changed_paths(
    base_ref: Option<&str>,
    diff_range_mode: DiffRangeMode,
) -> Result<Vec<String>, String> {
    let mut changed = BTreeSet::new();

    let mut command_sets = Vec::new();
    if let Some(base_ref) = base_ref {
        let range_sep = if matches!(diff_range_mode, DiffRangeMode::MergeBase) {
            "..."
        } else {
            ".."
        };
        command_sets.push(vec![
            "diff".to_string(),
            "--name-only".to_string(),
            "--relative".to_string(),
            format!("{base_ref}{range_sep}HEAD"),
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
    match &options.base_selector {
        BaseSelector::Explicit(base_ref) => Ok(Some(base_ref.clone())),
        BaseSelector::None => Ok(None),
        BaseSelector::AgainstMaster => {
            if git_ref_exists("origin/master")? {
                Ok(Some("origin/master".to_string()))
            } else {
                Err(
                    "--against-master requested but origin/master does not exist locally"
                        .to_string(),
                )
            }
        }
    }
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

pub fn parse_rebuild_options(arguments: &[String]) -> Result<RebuildOptions, String> {
    let mut options = RebuildOptions::default();
    let mut positional_mode = false;
    let mut saw_base = false;
    let mut saw_against_master = false;

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
                    options.output_mode = OutputMode::Json;
                    index += 1;
                    continue;
                }
                "--changed" => {
                    options.mode = RebuildMode::Changed;
                    index += 1;
                    continue;
                }
                "--merge-base" => {
                    options.diff_range_mode = DiffRangeMode::MergeBase;
                    index += 1;
                    continue;
                }
                "--dry-run" => {
                    options.execution_mode = ExecutionMode::DryRun;
                    index += 1;
                    continue;
                }
                "--against-master" => {
                    options.base_selector = BaseSelector::AgainstMaster;
                    saw_against_master = true;
                    index += 1;
                    continue;
                }
                "--all-workspace-plugins" => {
                    options.workspace_flag = WorkspaceFlag::ExplicitAll;
                    index += 1;
                    continue;
                }
                "--base" => {
                    let Some(base_ref) = arguments.get(index + 1) else {
                        return Err("--base requires a git ref argument".to_string());
                    };
                    options.base_selector = BaseSelector::Explicit(base_ref.clone());
                    saw_base = true;
                    index += 2;
                    continue;
                }
                value if value.starts_with("--base=") => {
                    let base_ref = value.trim_start_matches("--base=").trim();
                    if base_ref.is_empty() {
                        return Err("--base requires a non-empty git ref".to_string());
                    }
                    options.base_selector = BaseSelector::Explicit(base_ref.to_string());
                    saw_base = true;
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

    if saw_base && saw_against_master {
        return Err("--base and --against-master cannot be used together".to_string());
    }

    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::{build_rebuild_target_selection, parse_rebuild_options};
    use crate::{BaseSelector, BuildProfile, RebuildMode, RebuildOptions};

    #[test]
    fn parse_rebuild_options_supports_base_and_against_master() {
        let options = parse_rebuild_options(&[
            "--changed".to_string(),
            "--base".to_string(),
            "origin/master".to_string(),
        ])
        .expect("options should parse");
        assert!(matches!(options.mode, RebuildMode::Changed));
        assert!(matches!(options.base_selector, BaseSelector::Explicit(_)));

        let against_master =
            parse_rebuild_options(&["--changed".to_string(), "--against-master".to_string()])
                .expect("against-master should parse");
        assert!(matches!(
            against_master.base_selector,
            BaseSelector::AgainstMaster
        ));
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

    #[test]
    fn build_rebuild_target_selection_marks_selector_reason() {
        let options = RebuildOptions {
            profile: BuildProfile::Debug,
            selectors: vec!["bmux_windows_plugin".to_string()],
            ..RebuildOptions::default()
        };
        let selected =
            build_rebuild_target_selection(&options, &["bmux_windows_plugin".to_string()]);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].reason, "selector");
    }
}
