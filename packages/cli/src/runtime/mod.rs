use crate::connection::{
    ConnectionContext, ConnectionPolicyScope, ServerRuntimeMetadata, connect, connect_raw,
    connect_raw_with_context, connect_with_context, current_cli_build_id,
    expand_bmux_target_if_needed, map_client_connect_error, read_server_runtime_metadata,
    remove_server_runtime_metadata_file, write_server_runtime_metadata,
};
use crate::input::{InputProcessor, Keymap, RuntimeAction};
use anyhow::{Context, Result};
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, push_process_config_overrides};
use std::time::{Duration, Instant};
use uuid::Uuid;

mod action_dispatch;
pub mod attach;
mod attach_selected;
#[cfg(unix)]
mod bare_terminal;
mod bootstrap;
mod built_in_commands;
mod cli_parse;
mod config_cli;
mod device_seal_broker;
mod diagnostics_layer;
mod dispatch;
mod doctor_cli;
mod hosted_output;
#[cfg(feature = "bundled-plugin-connections")]
mod kiosk_cli;
mod logs_cli;
mod logs_watch;
mod managed_companion;
mod perf_telemetry;
mod plugin_commands;
mod plugin_host;
mod plugin_kernel;
mod plugin_runtime;
pub use plugin_runtime::RuntimeCommandState;
#[cfg(feature = "docs-test")]
pub use plugin_runtime::bundled_cli_command_for_docs;
mod prompt;
mod remote_cli;
mod sandbox_cli;
mod server_autostart;
mod server_commands;
mod server_runtime;
mod session_cli;
mod session_follow;
pub mod slot;
mod slot_cli;
mod startup_ownership;
mod terminal_doctor;
mod terminal_protocol;
mod web_attach;

use self::logs_watch::active_log_file_path;
pub use action_dispatch::{ActionDispatchError, ActionDispatchRequest};
pub use attach::runtime::AttachRunOutcome;
use attach::runtime::run_session_attach_with_client;
pub use attach::state::AttachExitReason;
use bootstrap::{
    DefaultAttachOptions, LogProcessScope, init_logging, map_attach_client_error,
    map_cli_client_error, run_default_server_attach, run_server_start, run_session_attach,
};
use built_in_commands::{BuiltInHandlerId, built_in_command_by_handler};
use cli_parse::{ParsedRuntimeCli, parse_runtime_cli, resolve_log_level, tracing_level};
use config_cli::{
    run_config_get, run_config_path, run_config_profiles_diff, run_config_profiles_evaluate,
    run_config_profiles_explain, run_config_profiles_lint, run_config_profiles_list,
    run_config_profiles_resolve, run_config_profiles_set_active_at_path, run_config_profiles_show,
    run_config_profiles_switch, run_config_scope_explain, run_config_set, run_config_show,
};
use device_seal_broker::run_device_seal_broker;
use dispatch::run_command;
use doctor_cli::run_doctor;
use logs_cli::{run_logs_level, run_logs_path};
pub use plugin_kernel::KernelClientFactory;
use plugin_kernel::{
    EFFECTIVE_LOG_LEVEL, LOG_CONTROL, available_capability_providers,
    available_service_descriptors, core_provided_capabilities, enter_host_kernel_client_factory,
    enter_host_kernel_connection, host_kernel_bridge, register_plugin_service_handlers,
    service_descriptors_from_declarations,
};
use plugin_runtime::{
    activate_loaded_plugins, build_runtime_command_state, command_accepts_repeat,
    deactivate_loaded_plugins, dispatch_loaded_plugin_event, effective_enabled_plugins,
    install_enabled_client_adapters, load_enabled_plugins, load_plugin,
    plugin_command_policy_hints, plugin_event_bridge_loop, plugin_host_metadata,
    plugin_system_event, resolve_plugin_search_paths, run_external_plugin_command,
    run_plugin_keybinding_command, run_plugin_keybinding_command_with_state,
    scan_available_plugins, validate_enabled_plugins,
};
pub use prompt::{
    PromptEvent, PromptField, PromptFormField, PromptFormLayout, PromptFormSection,
    PromptFormValue, PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptSubmitError,
    PromptValidation, PromptValue, PromptWidth,
};
use remote_cli::{
    SSH_RECONNECT_MAX_ATTEMPTS, connect_attach_target_with_kernel, reconnect_backoff_ms,
    run_target_proxy_from_current_argv, should_proxy_to_target,
};
use sandbox_cli::{
    BundleIncludeOptions, BundleSandboxOptions, InspectTargetOptions, RerunSandboxOptions,
    RunSandboxOptions, TriageBundleOptions, TriageSandboxOptions, run_sandbox_bundle,
    run_sandbox_cleanup, run_sandbox_doctor, run_sandbox_inspect, run_sandbox_list,
    run_sandbox_open, run_sandbox_rebuild_index, run_sandbox_rerun, run_sandbox_run,
    run_sandbox_status, run_sandbox_tail, run_sandbox_triage, run_sandbox_verify_bundle,
};
use server_commands::{
    run_server_bridge, run_server_gateway, run_server_status, run_server_stop,
    run_server_whoami_principal, server_event_name,
};
use server_runtime::{
    cleanup_stale_pid_file, fetch_server_status, read_server_pid_file, remove_server_pid_file,
    server_is_running, wait_for_server_running, wait_until_server_stopped, write_server_pid_file,
};

/// Re-export of [`server_runtime::is_pid_running`] for cross-module callers
/// (e.g. `crate::connection`).
pub fn is_pid_running_crate(pid: u32) -> anyhow::Result<bool> {
    server_runtime::is_pid_running(pid)
}
use session_cli::attach_quit_failure_status;
use session_follow::{parse_session_selector, parse_uuid_value, run_session_detach};
use terminal_doctor::{
    check_terminfo_available, merged_runtime_keybindings, resolve_pane_term, run_keymap_doctor,
    run_keymap_explain, run_terminal_doctor, run_terminal_install_terminfo, terminal_profile_name,
};
use terminal_protocol::{
    ProtocolDirection, ProtocolProfile, ProtocolTraceEvent, primary_da_for_profile,
    protocol_profile_name, secondary_da_for_profile, supported_query_names,
};

const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(200);
const SERVER_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
const ATTACH_SCROLLBACK_UNAVAILABLE_STATUS: &str = "scrollback unavailable for focused pane";
const ATTACH_SELECTION_STARTED_STATUS: &str = "selection started";
const ATTACH_SELECTION_CLEARED_STATUS: &str = "selection cleared";
const ATTACH_SELECTION_COPIED_STATUS: &str = "selection copied";
const ATTACH_SELECTION_EMPTY_STATUS: &str = "no selection";
const ATTACH_TRANSIENT_STATUS_TTL: Duration = Duration::from_millis(1800);
const HELP_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(1);

pub fn active_runtime_name() -> String {
    std::env::var("BMUX_RUNTIME_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

pub fn initialize_native_plugins(
    config: &BmuxConfig,
    paths: &bmux_config::ConfigPaths,
) -> Result<()> {
    plugin_runtime::initialize_native_runtime_state(config.clone(), paths.clone())
}

pub async fn run_attach_with_client(
    client: BmuxClient,
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<AttachRunOutcome> {
    run_session_attach_with_client(client, target, follow, global, None).await
}

pub fn submit_prompt_request(
    request: PromptRequest,
) -> std::result::Result<tokio::sync::oneshot::Receiver<PromptResponse>, PromptSubmitError> {
    prompt::submit(request)
}

pub fn submit_prompt_request_with_events(
    request: PromptRequest,
) -> std::result::Result<
    (
        tokio::sync::oneshot::Receiver<PromptResponse>,
        tokio::sync::mpsc::UnboundedReceiver<PromptEvent>,
    ),
    PromptSubmitError,
> {
    prompt::submit_with_events(request)
}

pub async fn request_prompt_response(
    request: PromptRequest,
) -> std::result::Result<PromptResponse, PromptSubmitError> {
    prompt::request(request).await
}

pub async fn request_prompt_response_with_events(
    request: PromptRequest,
) -> std::result::Result<
    (
        PromptResponse,
        tokio::sync::mpsc::UnboundedReceiver<PromptEvent>,
    ),
    PromptSubmitError,
> {
    let (response_rx, event_rx) = prompt::request_with_events(request)?;
    let response = response_rx
        .await
        .map_err(|_| PromptSubmitError::HostDisconnected)?;
    Ok((response, event_rx))
}

pub fn dispatch_action(action: impl Into<String>) -> std::result::Result<(), ActionDispatchError> {
    action_dispatch::dispatch(action)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProfile {
    Bmux256Color,
    Screen256Color,
    Xterm256Color,
    Conservative,
}

/// Returns `true` for commands that enter raw terminal mode (TUI attach
/// sessions).  When `command` is `None` the default attach path is taken,
/// which always enters raw mode.
fn logging_scope_for_command(command: Option<&bmux_cli_schema::Command>) -> LogProcessScope {
    use bmux_cli_schema::{Command, ServerCommand};
    let Some(cmd) = command else {
        return LogProcessScope::Client;
    };
    match cmd {
        Command::External(args)
            if args
                .first()
                .is_some_and(|s| matches!(s.as_str(), "connect" | "join" | "host")) =>
        {
            LogProcessScope::Client
        }
        Command::Attach { .. } => LogProcessScope::Client,
        Command::External(args) if matches!(args.as_slice(), [group, leaf, ..] if matches!(group.as_str(), "session" | "kiosk") && leaf == "attach") => {
            LogProcessScope::Client
        }
        Command::Server {
            command: ServerCommand::Start { .. } | ServerCommand::Gateway { .. },
        } => LogProcessScope::Server,
        _ => LogProcessScope::Command,
    }
}

pub fn set_logging_client_id(client_id: impl Into<String>) {
    if let Some(control) = LOG_CONTROL.get() {
        control.set_subject_id(client_id);
    }
}

fn command_enters_raw_mode(command: Option<&bmux_cli_schema::Command>) -> bool {
    use bmux_cli_schema::Command;
    let Some(cmd) = command else {
        // Default path (no subcommand) = attach to server.
        return true;
    };
    if let Command::External(args) = cmd
        && matches!(args.as_slice(), [group, leaf, ..] if group == "playbook" && matches!(leaf.as_str(), "interactive" | "run"))
    {
        return true;
    }
    if let Command::External(args) = cmd
        && args
            .first()
            .is_some_and(|s| matches!(s.as_str(), "connect" | "join" | "host"))
    {
        return true;
    }
    if matches!(cmd, Command::External(args) if matches!(args.as_slice(), [group, leaf, ..] if group == "session" && leaf == "attach"))
    {
        return true;
    }
    matches!(cmd, Command::Attach { .. })
}

/// Print a startup banner on stderr when:
/// - a slot is active,
/// - the slot differs from the manifest's presentational default,
/// - the command enters raw-mode (interactive attach-like flows),
/// - `BMUX_SLOT_BANNER` is not "off",
/// - stderr is a TTY.
fn maybe_print_slot_startup_banner(command: Option<&bmux_cli_schema::Command>) {
    use std::io::IsTerminal as _;
    if std::env::var("BMUX_SLOT_BANNER").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off")) {
        return;
    }
    if !command_enters_raw_mode(command) {
        return;
    }
    if !std::io::stderr().is_terminal() {
        return;
    }
    let state = slot::active_slot();
    let slot::ActiveSlotState::Resolved { slot, manifest, .. } = state else {
        return;
    };
    let default_name = manifest.resolved_default().map_or("", |s| s.name.as_str());
    if slot.name == default_name {
        return;
    }
    eprintln!("[bmux slot={} @ {}]", slot.name, env!("CARGO_PKG_VERSION"));
}

pub use dispatch::startup_future_marker;

pub async fn run(runtime_owner: Option<crate::runtime_owner::RuntimeOwner>) -> Result<u8> {
    startup_future_marker("runtime.before_parse", 0);
    let parsed = parse_runtime_cli(runtime_owner.as_ref())?;
    startup_future_marker("runtime.after_parse", 0);
    match parsed {
        ParsedRuntimeCli::BuiltIn {
            cli,
            log_level,
            verbose,
            config_overrides,
            command_state,
        } => {
            let _config_override_guard = push_process_config_overrides(config_overrides);
            // The command executes with the state this process resolved at
            // parse time (including its runtime owner); legacy readers on this
            // thread observe that explicit selection, never a lazily rebuilt one.
            let _selected_command_state = command_state
                .as_ref()
                .map(plugin_runtime::SelectedCommandStateGuard::enter);
            let file_only = command_enters_raw_mode(cli.command.as_ref()) || cli.core_builtins_only;
            let logging_scope = logging_scope_for_command(cli.command.as_ref());
            startup_future_marker("runtime.before_logging", 0);
            let _logging_guard = init_logging(verbose, Some(log_level), file_only, logging_scope);
            startup_future_marker("runtime.after_logging", 0);
            tracing::info!(
                scope = ?logging_scope,
                file_only,
                has_command = cli.command.is_some(),
                core_builtins_only = cli.core_builtins_only,
                "runtime.cli.builtin_start"
            );
            maybe_print_slot_startup_banner(cli.command.as_ref());
            let connection_context = ConnectionContext::new(cli.target.as_deref());
            if should_proxy_to_target(&cli).await? {
                return run_target_proxy_from_current_argv(&cli).await;
            }

            if let Some(command) = &cli.command {
                startup_future_marker("runtime.before_run_command", 0);
                return Box::pin(run_command(
                    command,
                    connection_context,
                    command_state.as_ref(),
                ))
                .await;
            }

            run_default_server_attach(DefaultAttachOptions, connection_context).await
        }
        ParsedRuntimeCli::Plugin {
            target,
            log_level,
            plugin_id,
            command_name,
            arguments,
            command_state,
            config_overrides,
        } => {
            let _config_override_guard = push_process_config_overrides(config_overrides);
            let _logging_guard =
                init_logging(false, Some(log_level), false, LogProcessScope::Command);
            tracing::info!(plugin_id, command_name, "runtime.cli.plugin_start");
            plugin_runtime::run_plugin_command_with_target(
                command_state.as_ref(),
                &plugin_id,
                &command_name,
                &arguments,
                target.as_deref(),
            )
            .await
        }
        ParsedRuntimeCli::ImmediateExit {
            code,
            output,
            stderr,
        } => {
            if stderr {
                eprint!("{output}");
            } else {
                print!("{output}");
            }
            Ok(code)
        }
    }
}

// ── Playbook commands ────────────────────────────────────────────────────────
