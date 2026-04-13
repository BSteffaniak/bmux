use crate::connection::{
    ConnectionContext, ConnectionPolicyScope, ServerRuntimeMetadata, connect,
    connect_if_running_with_context, connect_raw, connect_raw_with_context, connect_with_context,
    current_cli_build_id, expand_bmux_target_if_needed, map_client_connect_error,
    read_server_runtime_metadata, remove_server_runtime_metadata_file,
    write_server_runtime_metadata,
};
use crate::input::{InputProcessor, Keymap, RuntimeAction};
use anyhow::{Context, Result};
use bmux_cli_schema::{
    RecordingCursorBlinkMode, RecordingCursorMode, RecordingCursorPaintMode,
    RecordingCursorProfile, RecordingCursorShape, RecordingCursorTextMode, RecordingEventKindArg,
    RecordingExportFormat, RecordingListOrderArg, RecordingListSortArg, RecordingListStatusArg,
    RecordingPaletteSource, RecordingProfileArg, RecordingRenderMode, RecordingReplayMode,
};
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths, push_process_config_overrides};
use bmux_ipc::{RecordingEventEnvelope, RecordingEventKind, RecordingStatus, RecordingSummary};
use bmux_server::offline_kill_sessions;
use crossterm::terminal;
use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod access_cli;
mod action_dispatch;
mod attach;
mod bootstrap;
mod built_in_commands;
mod cli_parse;
mod config_cli;
mod dispatch;
mod doctor_cli;
mod logs_cli;
mod logs_watch;
mod perf_cli;
mod playbook_cli;
mod plugin_commands;
mod plugin_host;
mod plugin_kernel;
mod plugin_runtime;
mod prompt;
mod recording;
mod recording_cli;
mod remote_cli;
mod sandbox_cli;
mod server_commands;
mod server_runtime;
mod session_cli;
mod session_follow;
mod terminal_doctor;
mod terminal_protocol;

use self::logs_watch::{
    active_log_file_path, run_logs_profiles_delete, run_logs_profiles_list,
    run_logs_profiles_rename, run_logs_profiles_show, run_logs_watch,
};
use access_cli::{
    run_access_add, run_access_disable, run_access_enable, run_access_init, run_access_list,
    run_access_remove, run_access_status,
};
pub use action_dispatch::{ActionDispatchError, ActionDispatchRequest};
pub use attach::runtime::AttachRunOutcome;
use attach::runtime::run_session_attach_with_client;
pub use attach::state::AttachExitReason;
use bootstrap::{
    DefaultAttachOptions, init_logging, map_attach_client_error, map_cli_client_error,
    run_default_server_attach, run_server_start, run_session_attach,
};
use built_in_commands::{BuiltInHandlerId, built_in_command_by_handler};
use cli_parse::{
    ParsedRuntimeCli, parse_runtime_cli, resolve_log_level, tracing_level,
    validate_record_bootstrap_flags,
};
use config_cli::{
    run_config_get, run_config_path, run_config_profiles_diff, run_config_profiles_evaluate,
    run_config_profiles_explain, run_config_profiles_lint, run_config_profiles_list,
    run_config_profiles_resolve, run_config_profiles_set_active_at_path, run_config_profiles_show,
    run_config_profiles_switch, run_config_set, run_config_show,
};
use dispatch::run_command;
use doctor_cli::run_doctor;
use logs_cli::{run_logs_level, run_logs_path, run_logs_tail};
use perf_cli::{run_perf_off, run_perf_on, run_perf_status};
use playbook_cli::{
    run_playbook_cleanup, run_playbook_diff, run_playbook_dry_run, run_playbook_from_recording,
    run_playbook_interactive, run_playbook_run, run_playbook_validate,
};
use plugin_kernel::{
    EFFECTIVE_LOG_LEVEL, KernelClientFactory, LOG_WRITER_GUARD, available_capability_providers,
    available_service_descriptors, begin_host_kernel_effect_capture, core_provided_capabilities,
    enter_host_kernel_client_factory, enter_host_kernel_connection,
    finish_host_kernel_effect_capture, host_kernel_bridge, register_plugin_service_handlers,
    service_descriptors_from_declarations,
};
use plugin_runtime::{
    activate_loaded_plugins, bundled_plugin_root as bundled_plugin_roots,
    deactivate_loaded_plugins, discover_bundled_plugin_ids, dispatch_loaded_plugin_event,
    effective_enabled_plugins, load_enabled_plugins, load_plugin, plugin_command_policy_hints,
    plugin_event_bridge_loop, plugin_host_metadata, plugin_system_event,
    registered_plugin_entry_exists, resolve_plugin_search_paths, run_external_plugin_command,
    run_plugin_command, run_plugin_keybinding_command, scan_available_plugins,
    validate_enabled_plugins,
};
pub use prompt::{
    PromptField, PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptSubmitError,
    PromptValidation, PromptValue, PromptWidth,
};
use recording_cli::{
    recording_event_kind_name, replay_interactive, replay_verify, replay_watch,
    run_recording_analyze, run_recording_cut, run_recording_delete, run_recording_delete_all,
    run_recording_export, run_recording_inspect, run_recording_list, run_recording_path,
    run_recording_replay, run_recording_start, run_recording_status, run_recording_stop,
    run_recording_verify_smoke, verify_recording_report,
};
use remote_cli::{
    maybe_run_cluster_plugin_command_via_gateway, run_auth_login, run_auth_logout, run_auth_status,
    run_connect, run_host, run_hosts, run_join, run_remote_complete_sessions,
    run_remote_complete_targets, run_remote_doctor, run_remote_init, run_remote_install_server,
    run_remote_list, run_remote_test, run_remote_upgrade, run_setup, run_share,
    run_target_proxy_from_current_argv, run_unshare, should_proxy_to_target,
};
use sandbox_cli::{
    InspectTargetOptions, RerunSandboxOptions, RunSandboxOptions, TriageSandboxOptions,
    run_sandbox_bundle, run_sandbox_cleanup, run_sandbox_doctor, run_sandbox_inspect,
    run_sandbox_list, run_sandbox_open, run_sandbox_rebuild_index, run_sandbox_rerun,
    run_sandbox_run, run_sandbox_status, run_sandbox_tail, run_sandbox_triage,
};
use server_commands::{
    run_server_bridge, run_server_gateway, run_server_recording_clear, run_server_recording_path,
    run_server_recording_start, run_server_recording_status, run_server_recording_stop,
    run_server_restore, run_server_save, run_server_status, run_server_stop,
    run_server_whoami_principal, server_event_name,
};
use server_runtime::{
    cleanup_stale_pid_file, fetch_server_status, is_pid_running, parse_pid_content,
    read_server_pid_file, remove_server_pid_file, server_is_running, try_kill_pid,
    wait_for_process_exit, wait_for_server_running, wait_until_server_stopped,
    write_server_pid_file,
};
use session_cli::{
    attach_quit_failure_status, run_client_list, run_session_kill, run_session_kill_all,
    run_session_list, run_session_new,
};
use session_follow::{
    parse_session_selector, parse_uuid_value, run_follow, run_session_detach, run_unfollow,
};
use terminal_doctor::{
    check_terminfo_available, merged_runtime_keybindings, resolve_pane_term, run_keymap_doctor,
    run_keymap_explain, run_terminal_doctor, run_terminal_install_terminfo, terminal_profile_name,
};
use terminal_protocol::{
    ProtocolDirection, ProtocolProfile, ProtocolTraceEvent, primary_da_for_profile,
    protocol_profile_name, secondary_da_for_profile, supported_query_names,
};

const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(200);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_millis(5000);
const VERIFY_SERVER_START_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);
const ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE: usize = 512 * 1024;
const ATTACH_SCROLLBACK_UNAVAILABLE_STATUS: &str = "scrollback unavailable for focused pane";
const ATTACH_SELECTION_STARTED_STATUS: &str = "selection started";
const ATTACH_SELECTION_CLEARED_STATUS: &str = "selection cleared";
const ATTACH_SELECTION_COPIED_STATUS: &str = "selection copied";
const ATTACH_SELECTION_EMPTY_STATUS: &str = "no selection";
const ATTACH_TRANSIENT_STATUS_TTL: Duration = Duration::from_millis(1800);
const ATTACH_WELCOME_STATUS_TTL: Duration = Duration::from_millis(2600);
const HELP_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(1);

pub fn active_runtime_name() -> String {
    std::env::var("BMUX_RUNTIME_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

pub fn append_runtime_arg(command: &mut ProcessCommand) {
    command.arg("--runtime").arg(active_runtime_name());
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

pub async fn request_prompt_response(
    request: PromptRequest,
) -> std::result::Result<PromptResponse, PromptSubmitError> {
    prompt::request(request).await
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
const fn command_enters_raw_mode(command: Option<&bmux_cli_schema::Command>) -> bool {
    use bmux_cli_schema::{
        Command, LogsCommand, PlaybookCommand, RecordingCommand, SessionCommand,
    };
    let Some(cmd) = command else {
        // Default path (no subcommand) = attach to server.
        return true;
    };
    matches!(
        cmd,
        Command::Attach { .. }
            | Command::Connect { .. }
            | Command::Join { .. }
            | Command::Host { .. }
            | Command::Session {
                command: SessionCommand::Attach { .. }
            }
            | Command::Recording {
                command: RecordingCommand::Replay { .. }
            }
            | Command::Playbook {
                command: PlaybookCommand::Run { .. } | PlaybookCommand::Interactive { .. }
            }
            | Command::Logs {
                command: LogsCommand::Watch { .. }
            }
    )
}

pub async fn run() -> Result<u8> {
    match parse_runtime_cli()? {
        ParsedRuntimeCli::BuiltIn {
            cli,
            log_level,
            verbose,
            config_overrides,
        } => {
            let _config_override_guard = push_process_config_overrides(config_overrides);
            let file_only = command_enters_raw_mode(cli.command.as_ref()) || cli.core_builtins_only;
            init_logging(verbose, Some(log_level), file_only);
            validate_record_bootstrap_flags(&cli)?;
            let connection_context = ConnectionContext::new(cli.target.as_deref());
            if should_proxy_to_target(&cli).await? {
                return run_target_proxy_from_current_argv(&cli).await;
            }

            if let Some(command) = &cli.command {
                return run_command(command, connection_context).await;
            }

            let options = DefaultAttachOptions {
                record: cli.record,
                capture_input: !cli.no_capture_input,
                profile: cli.record_profile,
                name: cli.record_name.clone(),
                event_kinds: cli.record_event_kind.clone(),
                recording_id_file: cli.recording_id_file.clone(),
                stop_server_on_exit: cli.stop_server_on_exit,
            };
            run_default_server_attach(options, connection_context).await
        }
        ParsedRuntimeCli::Plugin {
            log_level,
            plugin_id,
            command_name,
            arguments,
            config_overrides,
        } => {
            let _config_override_guard = push_process_config_overrides(config_overrides);
            init_logging(false, Some(log_level), false);
            if let Some(status) =
                maybe_run_cluster_plugin_command_via_gateway(&plugin_id, &command_name, &arguments)
                    .await?
            {
                return Ok(status);
            }
            run_plugin_command(&plugin_id, &command_name, &arguments).await
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
