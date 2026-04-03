use crate::connection::{
    ConnectionContext, ConnectionPolicyScope, ServerRuntimeMetadata, connect,
    connect_if_running_with_context, connect_raw, connect_raw_with_context, connect_with_context,
    current_cli_build_id, expand_bmux_target_if_needed, map_client_connect_error,
    read_server_runtime_metadata, remove_server_runtime_metadata_file,
    write_server_runtime_metadata,
};
use crate::input::{InputProcessor, Keymap, RuntimeAction};
use crate::status::{AttachStatusLine, AttachTab, build_attach_status_line};
use anyhow::{Context, Result};
use bmux_cli_schema::{
    AuthCommand, Cli, Command, ConfigCommand, KeymapCommand, LogLevel, LogsCommand,
    LogsProfilesCommand, PlaybookCommand, RecordingCommand, RecordingCursorBlinkMode,
    RecordingCursorMode, RecordingCursorPaintMode, RecordingCursorProfile, RecordingCursorShape,
    RecordingCursorTextMode, RecordingEventKindArg, RecordingExportFormat, RecordingProfileArg,
    RecordingRenderMode, RecordingReplayMode, RemoteCommand, RemoteCompleteCommand, ServerCommand,
    SessionCommand, TerminalCommand, TraceFamily,
};
use bmux_client::{
    AttachLayoutState, AttachSnapshotState, BmuxClient, ClientError, StreamingBmuxClient,
};
use bmux_config::{
    BmuxConfig, ConfigPaths, PaneRestoreMethod, RecordingExportCursorBlinkMode,
    RecordingExportCursorMode, RecordingExportCursorPaintMode, RecordingExportCursorProfile,
    RecordingExportCursorShape, RecordingExportCursorTextMode, ResolvedTimeout, StatusPosition,
    TerminfoAutoInstall,
};
use bmux_ipc::{
    AttachRect, AttachViewComponent, ContextSelector, ContextSummary, InvokeServiceKind,
    PaneFocusDirection, PaneSelector, PaneSplitDirection, RecordingEventEnvelope,
    RecordingEventKind, RecordingPayload, RecordingStatus, RecordingSummary, SessionSelector,
    SessionSummary,
};
use bmux_keybind::action_to_config_name;
use bmux_plugin::{
    PluginManifest, PluginRegistry, load_registered_plugin as load_native_registered_plugin,
};
use bmux_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
    HostScope, NativeCommandContext, NativeLifecycleContext, PluginCommandEffect,
    PluginCommandOutcome, PluginEvent, PluginEventKind, RegisteredService, ServiceKind,
    ServiceRequest,
};
use bmux_server::{BmuxServer, OfflineSessionKillTarget, offline_kill_sessions};
use clap::{CommandFactory, FromArgMatches};
use crossterm::cursor::{MoveTo, SavePosition, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::terminal::{Clear, ClearType};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use std::cell::RefCell;
use std::io::{self, BufWriter, IsTerminal, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::{Duration as TimeDuration, OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::{Level, debug, trace, warn};
use uuid::Uuid;

use bmux_server::ServiceInvokeContext;

mod attach;
mod bootstrap;
mod built_in_commands;
mod cli_parse;
mod config_cli;
mod dispatch;
mod doctor_cli;
mod logs_cli;
mod logs_watch;
mod playbook_cli;
mod plugin_commands;
mod plugin_host;
mod plugin_kernel;
mod plugin_runtime;
mod recording;
mod recording_cli;
mod remote_cli;
mod server_commands;
mod server_runtime;
mod session_cli;
mod session_follow;
mod terminal_doctor;
mod terminal_protocol;

#[cfg(test)]
pub use self::logs_watch::{
    LogFilterCaseMode, LogFilterKind, LogFilterRule, compile_filter_regex, line_visible_in_watch,
    logs_watch_filter_rule_to_state, logs_watch_filter_state_to_rule, normalize_logs_watch_profile,
};
use self::logs_watch::{
    active_log_file_path, run_logs_profiles_delete, run_logs_profiles_list,
    run_logs_profiles_rename, run_logs_profiles_show, run_logs_watch,
};
use attach::cursor::apply_attach_cursor_state;
use attach::events::{AttachLoopControl, AttachLoopEvent};
use attach::render::{
    AttachLayer, AttachLayerSurface, append_pane_output, opaque_row_text, queue_layer_fill,
    render_attach_scene, visible_scene_pane_ids,
};
use attach::runtime::*;
use attach::state::{
    AttachEventAction, AttachExitReason, AttachScrollbackCursor, AttachScrollbackPosition,
    AttachUiMode, AttachViewState, PaneRect,
};
use bootstrap::*;
use built_in_commands::{BuiltInHandlerId, built_in_command_by_handler};
use cli_parse::*;
use config_cli::*;
use dispatch::*;
use doctor_cli::*;
use logs_cli::*;
use playbook_cli::*;
use plugin_commands::PluginCommandRegistry;
use plugin_kernel::*;
use plugin_runtime::*;
use recording_cli::*;
use remote_cli::*;
use server_commands::*;
use server_runtime::*;
use session_cli::*;
use session_follow::*;
use terminal_doctor::*;
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

pub(super) fn active_runtime_name() -> String {
    std::env::var("BMUX_RUNTIME_NAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

pub(super) fn append_runtime_arg(command: &mut ProcessCommand) {
    command.arg("--runtime").arg(active_runtime_name());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProfile {
    Bmux256Color,
    Screen256Color,
    Xterm256Color,
    Conservative,
}

pub async fn run() -> Result<u8> {
    match parse_runtime_cli()? {
        ParsedRuntimeCli::BuiltIn {
            cli,
            log_level,
            verbose,
        } => {
            init_logging(verbose, Some(log_level));
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
        } => {
            init_logging(false, Some(log_level));
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
