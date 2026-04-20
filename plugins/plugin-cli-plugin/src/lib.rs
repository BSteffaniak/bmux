#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod doctor;
mod list_cmd;
mod rebuild;
mod run_cmd;
mod suggest;

use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::{
    CoreCliCommandRequest, CoreCliCommandResponse, NativeCommandContext, PluginCommandError,
    RustPlugin,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct PluginCliPlugin;

impl RustPlugin for PluginCliPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        match context.command.as_str() {
            "list" => list_cmd::run_list_command(&context).map_err(PluginCommandError::from),
            "run" => run_cmd::run_run_command(&context).map_err(PluginCommandError::from),
            "rebuild" => rebuild::run_rebuild_command(&context).map_err(PluginCommandError::from),
            "doctor" => doctor::run_doctor_command(&context).map_err(PluginCommandError::from),
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

fn has_flag(arguments: &[String], long_name: &str) -> bool {
    let long_flag = format!("--{long_name}");
    arguments.iter().any(|argument| argument == &long_flag)
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

fn dir_name_to_crate_name(dir_name: &str) -> String {
    format!("bmux_{}", dir_name.replace('-', "_"))
}

#[derive(Debug, Default)]
struct RebuildOptions {
    profile: BuildProfile,
    output_mode: OutputMode,
    workspace_flag: WorkspaceFlag,
    execution_mode: ExecutionMode,
    mode: RebuildMode,
    diff_range_mode: DiffRangeMode,
    base_selector: BaseSelector,
    selectors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
enum OutputMode {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, Default)]
enum WorkspaceFlag {
    #[default]
    ImplicitAll,
    ExplicitAll,
}

#[derive(Debug, Clone, Copy, Default)]
enum ExecutionMode {
    #[default]
    Execute,
    DryRun,
}

#[derive(Debug, Clone, Copy, Default)]
enum DiffRangeMode {
    #[default]
    Direct,
    MergeBase,
}

#[derive(Debug, Clone, Default)]
enum BaseSelector {
    #[default]
    None,
    AgainstMaster,
    Explicit(String),
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
    selected_by: Vec<RebuildTargetSelection>,
}

#[derive(Debug, Serialize)]
struct RebuildTargetSelection {
    crate_name: String,
    reason: String,
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
    strict_mode: bool,
    enabled_plugins: Vec<String>,
    inspected_plugins: usize,
    findings: Vec<DoctorFinding>,
    error_count: usize,
    warning_count: usize,
    info_count: usize,
    issues: Vec<String>,
    warnings: Vec<String>,
    next_steps: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::{PluginCliPlugin, core_proxy_command_path};
    use bmux_plugin_sdk::{
        CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
        NativeCommandContext, RegisteredPluginInfo, RustPlugin,
    };
    use std::collections::BTreeMap;

    fn test_context(command: &str, arguments: Vec<String>) -> NativeCommandContext {
        NativeCommandContext {
            plugin_id: "bmux.plugin_cli".to_string(),
            command: command.to_string(),
            arguments,
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: vec!["bmux.commands".to_string()],
            enabled_plugins: vec!["bmux.example".to_string()],
            plugin_search_roots: Vec::new(),
            registered_plugins: vec![RegisteredPluginInfo {
                id: "bmux.example".to_string(),
                display_name: "Example".to_string(),
                version: "0.1.0".to_string(),
                bundled_static: true,
                required_capabilities: vec!["bmux.commands".to_string()],
                provided_capabilities: vec!["bmux.example.read".to_string()],
                commands: vec!["status".to_string(), "run".to_string()],
            }],
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "test".to_string(),
                plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: HostConnectionInfo {
                config_dir: "/tmp".to_string(),
                runtime_dir: "/tmp".to_string(),
                data_dir: "/tmp".to_string(),
                state_dir: "/tmp".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            host_kernel_bridge: None,
        }
    }

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
    fn integration_list_command_accepts_new_filter_flags() {
        let mut plugin = PluginCliPlugin;
        let context = test_context(
            "list",
            vec![
                "--json".to_string(),
                "--enabled-only".to_string(),
                "--capability=bmux.commands".to_string(),
            ],
        );
        let exit = plugin
            .run_command(context)
            .expect("list with filter flags should succeed");
        assert_eq!(exit, 0);
    }

    #[test]
    fn integration_doctor_command_accepts_summary_and_filters() {
        let mut plugin = PluginCliPlugin;
        let context = test_context(
            "doctor",
            vec![
                "--json".to_string(),
                "--summary-only".to_string(),
                "--severity=warning".to_string(),
                "--code=manifest".to_string(),
            ],
        );
        let exit = plugin
            .run_command(context)
            .expect("doctor with summary/filter flags should succeed");
        assert_eq!(exit, 0);
    }

    #[test]
    fn integration_run_help_succeeds_for_known_plugin() {
        let mut plugin = PluginCliPlugin;
        let context = test_context(
            "run",
            vec!["bmux.example".to_string(), "--help".to_string()],
        );
        let exit = plugin
            .run_command(context)
            .expect("run --help should succeed for known plugin");
        assert_eq!(exit, 0);
    }

    #[test]
    fn integration_doctor_invalid_severity_is_actionable() {
        let mut plugin = PluginCliPlugin;
        let context = test_context(
            "doctor",
            vec!["--severity=critical".to_string(), "--json".to_string()],
        );
        let error = plugin
            .run_command(context)
            .expect_err("invalid severity should fail");
        assert!(
            error
                .to_string()
                .contains("--severity must be one of: error, warning, info")
        );
    }
}

bmux_plugin_sdk::export_plugin!(PluginCliPlugin, include_str!("../plugin.toml"));
