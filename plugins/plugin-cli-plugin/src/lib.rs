#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/plugin-cli-plugin.bpdl",
        manifest: "plugin.toml",
    }
}

mod diagnostics_cli;
mod diagnostics_watch;
mod doctor;
mod list_cmd;
mod logs_tail;
mod logs_watch;
mod rebuild;
mod run_cmd;
mod suggest;

use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::{
    CoreCliCommandRequest, CoreCliCommandResponse, NativeCommandContext, PluginCommandError,
    perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_perf_phase},
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Default)]
pub struct PluginCliPlugin;

// Every command body is synchronous and runs inside its callback entry.
impl Plugin for PluginCliPlugin {
    fn declare(d: &mut Declarations<Self>) {
        for name in [
            "list",
            "run",
            "rebuild",
            "doctor",
            "logs-path",
            "logs-level",
            "diagnostics-watch",
            "logs-tail",
            "logs-watch",
            "logs-profiles-list",
            "logs-profiles-show",
            "logs-profiles-delete",
            "logs-profiles-rename",
            "keymap-doctor",
            "terminal-doctor",
            "terminal-install-terminfo",
        ] {
            d.blocking_command(name, |_, context| Self::command(context));
        }
    }
}

impl PluginCliPlugin {
    fn command(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        let started = Instant::now();
        let result = match context.command.as_str() {
            "list" => list_cmd::run_list_command(context).map_err(PluginCommandError::from),
            "run" => run_cmd::run_run_command(context).map_err(PluginCommandError::from),
            "diagnostics-watch"
            | "logs-path"
            | "logs-tail"
            | "logs-watch"
            | "logs-profiles-list"
            | "logs-profiles-show"
            | "logs-profiles-delete"
            | "logs-profiles-rename" => diagnostics_cli::run(context),
            "rebuild" => rebuild::run_rebuild_command(context).map_err(PluginCommandError::from),
            "doctor" => doctor::run_doctor_command(context).map_err(PluginCommandError::from),
            _ => core_proxy_command_path(context.command.as_str()).map_or_else(
                || {
                    Err(PluginCommandError::from(format!(
                        "unsupported command '{}'",
                        context.command
                    )))
                },
                |command_path| run_core_proxy_command(context, command_path),
            ),
        };
        emit_phase_timing(&context.command, started.elapsed().as_micros(), &result);
        result
    }
}

fn emit_phase_timing(command: &str, total_us: u128, result: &Result<i32, PluginCommandError>) {
    let payload = PhasePayload::new("bmux.plugin_cli.command")
        .field("plugin_id", "bmux.plugin_cli")
        .field("command_name", command)
        .field("total_us", total_us)
        .field("status", result.as_ref().copied().unwrap_or(1))
        .finish();
    emit_perf_phase(PhaseChannel::Plugin, &payload);
}

// These baseline aliases still depend on a core handler. New manifest entries must
// never acquire core execution authority implicitly. Remove a row and its manifest
// declaration together when the real owner registers that path.
const CORE_COMPAT_COMMANDS: &[(&str, &[&str])] = &[
    ("logs-level", &["logs", "level"]),
    ("keymap-doctor", &["keymap", "doctor"]),
    ("terminal-doctor", &["terminal", "doctor"]),
    (
        "terminal-install-terminfo",
        &["terminal", "install-terminfo"],
    ),
];

fn core_proxy_command_path(name: &str) -> Option<&'static [&'static str]> {
    CORE_COMPAT_COMMANDS
        .iter()
        .find_map(|&(alias, path)| (alias == name).then_some(path))
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
    if let Some(error) = response.error {
        return Err(PluginCommandError::new(response.exit_code.max(1), error));
    }
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
        NativeCommandContext, RegisteredPluginInfo,
    };
    use std::collections::BTreeMap;

    #[test]
    fn selected_command_rejects_missing_execution_before_any_host_effect() {
        use bmux_plugin_sdk::definition::PluginInstance;
        let plugin = bmux_plugin_sdk::author::DeclaredInstance::new(
            PluginCliPlugin,
            crate::declaration::PLUGIN_MANIFEST,
            crate::declaration::SERVICE_CONTRACTS,
        )
        .unwrap()
        .for_instance(bmux_plugin_sdk::definition::LoadedInstanceId::from_uuid(
            "00000000-0000-0000-0000-000000000001".parse().unwrap(),
        ));
        let call = bmux_plugin_sdk::definition::PluginCallContext {
            callback_execution: None,
            invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
            cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
            host_services: None,
        };
        let mut work = std::pin::pin!(plugin.command(call, test_context("list", Vec::new())));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(outcome) =
            std::future::Future::poll(work.as_mut(), &mut context)
        else {
            panic!("missing execution must reject before queueing");
        };
        assert_ne!(outcome.exit_code, 0);
        assert!(
            outcome
                .error
                .unwrap()
                .contains("callback execution authority missing")
        );
    }

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
                command_schemas: Vec::new(),
            }],
            active_keybindings: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "test".to_string(),
                plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: HostConnectionInfo {
                config_dir: "/tmp".to_string(),
                config_dir_candidates: vec!["/tmp".to_string()],
                runtime_dir: "/tmp".to_string(),
                data_dir: "/tmp".to_string(),
                state_dir: "/tmp".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: None,
            host_service_port: None,
        }
    }

    #[test]
    fn logs_path_runs_without_a_host_bridge() {
        let context = test_context("logs-path", vec!["--json".to_owned()]);
        assert_eq!(PluginCliPlugin::command(&context).unwrap(), 0);
        assert!(core_proxy_command_path("logs-path").is_none());
    }

    #[test]
    fn compatibility_routes_match_exact_ownership_claims() {
        use std::collections::BTreeSet;
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        let mut routed = BTreeSet::new();
        for command in manifest.commands {
            let path: Vec<_> = command.path.iter().map(String::as_str).collect();
            if command.summary.ends_with(" command proxy") {
                assert_eq!(
                    core_proxy_command_path(&command.name),
                    Some(path.as_slice())
                );
                assert!(
                    manifest
                        .owns_paths
                        .iter()
                        .any(|claim| claim.0 == command.path)
                );
                assert!(routed.insert(command.path));
            } else {
                assert!(core_proxy_command_path(&command.name).is_none());
            }
        }
        assert_eq!(routed.len(), super::CORE_COMPAT_COMMANDS.len());
    }

    #[test]
    fn integration_list_command_accepts_new_filter_flags() {
        let context = test_context(
            "list",
            vec![
                "--json".to_string(),
                "--enabled-only".to_string(),
                "--capability=bmux.commands".to_string(),
            ],
        );
        let exit =
            PluginCliPlugin::command(&context).expect("list with filter flags should succeed");
        assert_eq!(exit, 0);
    }

    #[test]
    fn integration_doctor_command_accepts_summary_and_filters() {
        let context = test_context(
            "doctor",
            vec![
                "--json".to_string(),
                "--summary-only".to_string(),
                "--severity=warning".to_string(),
                "--code=manifest".to_string(),
            ],
        );
        let exit = PluginCliPlugin::command(&context)
            .expect("doctor with summary/filter flags should succeed");
        assert_eq!(exit, 0);
    }

    #[test]
    fn integration_run_help_succeeds_for_known_plugin() {
        let context = test_context(
            "run",
            vec!["bmux.example".to_string(), "--help".to_string()],
        );
        let exit =
            PluginCliPlugin::command(&context).expect("run --help should succeed for known plugin");
        assert_eq!(exit, 0);
    }

    #[test]
    fn integration_doctor_invalid_severity_is_actionable() {
        let context = test_context(
            "doctor",
            vec!["--severity=critical".to_string(), "--json".to_string()],
        );
        let error = PluginCliPlugin::command(&context).expect_err("invalid severity should fail");
        assert!(
            error
                .to_string()
                .contains("--severity must be one of: error, warning, info")
        );
    }
}

bmux_plugin_sdk::plugin!(PluginCliPlugin);

#[cfg(test)]
mod canonical_metadata_tests {
    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        let definition = super::plugin_definition();
        definition
            .validate()
            .expect("canonical embedded identity must validate");
    }
}
