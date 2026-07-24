use crate::{
    auto_export_output_path, current_auto_export_settings, export,
    publish_recording_export_completed, publish_recording_export_failed,
    publish_recording_export_started, recordings_root_for_command,
};
use anyhow::{Context, Result};
use bmux_ipc::{InvokeServiceKind, Request, Response, ResponsePayload};
use bmux_plugin_sdk::{
    HostKernelBridge, HostKernelBridgeRequest, HostKernelBridgeResponse, NativeCommandContext,
    PluginCommandError, TypedDispatchClient, TypedDispatchClientError, TypedDispatchClientResult,
    decode_service_message, encode_service_message,
};
use bmux_recording_plugin_api::{recording_commands, recording_state, recording_types};
use bmux_recording_protocol::{
    RecordingEventEnvelope as ProtocolRecordingEventEnvelope, RecordingEventKind,
    RecordingPayload as ProtocolRecordingPayload, RecordingRollingClearReport,
    RecordingRollingStatus, RecordingStatus, RecordingSummary, read_frames,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

const KERNEL_STATUS_OK: i32 = 0;
const KERNEL_STATUS_BUFFER_TOO_SMALL: i32 = 4;

type RecordingPayload = ProtocolRecordingPayload<bmux_ipc::Event, bmux_ipc::ErrorCode>;
type RecordingEventEnvelope = ProtocolRecordingEventEnvelope<bmux_ipc::Event, bmux_ipc::ErrorCode>;

#[derive(Debug, Clone, serde::Serialize)]
struct StartRequest {
    session_id: Option<Uuid>,
    capture_input: bool,
    name: Option<String>,
    profile: Option<recording_types::RecordingProfile>,
    event_kinds: Option<Vec<recording_types::RecordingEventKind>>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct StopRequest {
    recording_id: Option<Uuid>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct DeleteRequest {
    recording_id: Uuid,
}

#[derive(Debug, Clone, serde::Serialize)]
struct PruneRequest {
    older_than_days: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplayMode {
    Watch,
    Interactive,
    Verify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListSort {
    Started,
    Name,
    Events,
    Size,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListStatus {
    All,
    Active,
    Done,
}

#[derive(Debug, Default)]
struct ArgCursor<'a> {
    args: &'a [String],
}

impl<'a> ArgCursor<'a> {
    fn new(args: &'a [String]) -> Self {
        Self { args }
    }

    fn positional(&self) -> Vec<&'a str> {
        let mut result = Vec::new();
        let mut iter = self.args.iter().map(String::as_str).peekable();
        while let Some(arg) = iter.next() {
            if arg == "--" {
                result.extend(iter);
                break;
            }
            if arg.starts_with("--") {
                if !arg.contains('=') && flag_takes_value(arg.trim_start_matches("--")) {
                    let _ = iter.next();
                }
                continue;
            }
            if arg == "-o" {
                let _ = iter.next();
                continue;
            }
            result.push(arg);
        }
        result
    }

    fn has_flag(&self, name: &str) -> bool {
        let long = format!("--{name}");
        self.args.iter().any(|arg| arg == &long)
    }

    fn option(&self, name: &str) -> Option<String> {
        let long = format!("--{name}");
        let prefix = format!("--{name}=");
        let mut iter = self.args.iter();
        while let Some(arg) = iter.next() {
            if let Some(value) = arg.strip_prefix(&prefix) {
                return Some(value.to_string());
            }
            if arg == &long {
                return iter.next().cloned();
            }
        }
        None
    }

    fn short_option(&self, name: &str) -> Option<String> {
        let short = format!("-{name}");
        let mut iter = self.args.iter();
        while let Some(arg) = iter.next() {
            if arg == &short {
                return iter.next().cloned();
            }
        }
        None
    }

    fn multi_options(&self, name: &str) -> Vec<String> {
        let long = format!("--{name}");
        let prefix = format!("--{name}=");
        let mut result = Vec::new();
        let mut iter = self.args.iter();
        while let Some(arg) = iter.next() {
            if let Some(value) = arg.strip_prefix(&prefix) {
                result.push(value.to_string());
            } else if arg == &long
                && let Some(value) = iter.next()
            {
                result.push(value.clone());
            }
        }
        result
    }

    fn u64_option(&self, name: &str) -> Result<Option<u64>> {
        self.option(name)
            .map(|value| {
                value
                    .parse::<u64>()
                    .with_context(|| format!("invalid --{name} value '{value}'"))
            })
            .transpose()
    }

    fn u32_option(&self, name: &str) -> Result<Option<u32>> {
        self.u64_option(name)?
            .map(|value| u32::try_from(value).context("value does not fit in u32"))
            .transpose()
    }

    fn usize_option(&self, name: &str) -> Result<Option<usize>> {
        self.u64_option(name)?
            .map(|value| usize::try_from(value).context("value does not fit in usize"))
            .transpose()
    }

    fn f64_option(&self, name: &str) -> Result<Option<f64>> {
        self.option(name)
            .map(|value| {
                value
                    .parse::<f64>()
                    .with_context(|| format!("invalid --{name} value '{value}'"))
            })
            .transpose()
    }
}

fn flag_takes_value(name: &str) -> bool {
    matches!(
        name.split_once('=').map_or(name, |(name, _)| name),
        "session-id"
            | "name"
            | "profile"
            | "kind"
            | "event-kind"
            | "rolling-window-secs"
            | "rolling-event-kind"
            | "last-seconds"
            | "export-fps"
            | "older-than"
            | "older-than-days"
            | "limit"
            | "sort"
            | "order"
            | "status"
            | "query"
            | "output"
            | "mode"
            | "speed"
            | "compare-recording"
            | "ignore"
    )
}

pub(crate) fn run_recording_command(
    context: &NativeCommandContext,
) -> std::result::Result<i32, PluginCommandError> {
    run_recording_command_inner(context)
        .map_err(|error| PluginCommandError::failed(error.to_string()))
}

fn run_recording_command_inner(context: &NativeCommandContext) -> Result<i32> {
    let args = ArgCursor::new(&context.arguments);
    match context.command.as_str() {
        "recording-start" => run_start(context, &args),
        "recording-stop" => run_stop(context, &args),
        "recording-status" => run_status(context, &args),
        "recording-path" => run_path(context, &args),
        "recording-list" => run_list(context, &args),
        "recording-delete" => run_delete(context, &args),
        "recording-delete-all" => run_delete_all(context, &args),
        "recording-cut" => run_cut(context, &args),
        "recording-inspect" => run_inspect(context, &args),
        "recording-analyze" => run_analyze(context, &args),
        "recording-replay" => run_replay(context, &args),
        "recording-verify-smoke" => run_verify_smoke(context, &args),
        "recording-prune" => run_prune(context, &args),
        "server-recording" => run_server_recording(context),
        "playbook-from-recording" => run_playbook_from_recording(context, &args),
        _ => Err(anyhow::anyhow!(
            "unknown recording command '{}'",
            context.command
        )),
    }
}

fn invoke_recording_service<RequestBody, ResponseBody>(
    context: &NativeCommandContext,
    capability: &str,
    kind: InvokeServiceKind,
    operation: &str,
    request: &RequestBody,
) -> Result<ResponseBody>
where
    RequestBody: serde::Serialize,
    ResponseBody: serde::de::DeserializeOwned,
{
    let payload = encode_service_message(request)
        .map_err(|error| anyhow::anyhow!("failed encoding recording service request: {error}"))?;
    let interface_id = match kind {
        InvokeServiceKind::Query => "recording-state",
        InvokeServiceKind::Command => "recording-commands",
    };
    let response = invoke_service_raw_bridge(
        context.host_kernel_bridge,
        capability,
        kind,
        interface_id,
        operation,
        payload,
    )?;
    decode_service_message(&response)
        .map_err(|error| anyhow::anyhow!("failed decoding recording service response: {error}"))
}

fn invoke_service_raw_bridge(
    bridge: Option<HostKernelBridge>,
    capability: &str,
    kind: InvokeServiceKind,
    interface_id: &str,
    operation: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>> {
    let request = Request::InvokeService {
        capability: capability.to_string(),
        kind,
        interface_id: interface_id.to_string(),
        operation: operation.to_string(),
        payload,
    };
    let response_payload = execute_kernel_request(bridge, &request)?;
    let ResponsePayload::ServiceInvoked { payload } = response_payload else {
        anyhow::bail!("recording service returned unexpected response shape");
    };
    Ok(payload)
}

struct HostBridgeTypedClient {
    bridge: Option<HostKernelBridge>,
}

impl TypedDispatchClient for HostBridgeTypedClient {
    async fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> TypedDispatchClientResult<Vec<u8>> {
        invoke_service_raw_bridge(
            self.bridge,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
        .map_err(|error| {
            TypedDispatchClientError::transport(interface_id, operation, error.to_string())
        })
    }
}

fn execute_kernel_request(
    bridge: Option<HostKernelBridge>,
    request: &Request,
) -> Result<ResponsePayload> {
    let bridge = bridge.context("recording command requires a running bmux server")?;
    let encoded_request = bmux_ipc::encode(request)
        .map_err(|error| anyhow::anyhow!("failed encoding kernel request: {error}"))?;
    let encoded_response = invoke_host_kernel_bridge(bridge, encoded_request)?;
    let response: Response = bmux_ipc::decode(&encoded_response)
        .map_err(|error| anyhow::anyhow!("failed decoding kernel response: {error}"))?;
    match response {
        Response::Ok(payload) => Ok(payload),
        Response::Err(error) => anyhow::bail!(error.message),
    }
}

fn invoke_host_kernel_bridge(bridge: HostKernelBridge, payload: Vec<u8>) -> Result<Vec<u8>> {
    let request = encode_service_message(&HostKernelBridgeRequest::new(payload))
        .map_err(|error| anyhow::anyhow!("failed encoding host bridge request: {error}"))?;
    let mut output = vec![0u8; request.len().saturating_mul(4).max(1024)];
    let mut output_len = 0usize;
    let status = bridge.invoke(
        request.as_ptr(),
        request.len(),
        output.as_mut_ptr(),
        output.len(),
        &raw mut output_len,
    );
    if status == KERNEL_STATUS_BUFFER_TOO_SMALL {
        output.resize(output_len, 0);
        let status = bridge.invoke(
            request.as_ptr(),
            request.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut output_len,
        );
        if status != KERNEL_STATUS_OK {
            anyhow::bail!("kernel bridge invocation failed with status {status}");
        }
    } else if status != KERNEL_STATUS_OK {
        anyhow::bail!("kernel bridge invocation failed with status {status}");
    }
    output.truncate(output_len);
    let response: HostKernelBridgeResponse = decode_service_message(&output)
        .map_err(|error| anyhow::anyhow!("failed decoding host bridge response: {error}"))?;
    Ok(response.payload)
}

fn run_start(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let session_id = args
        .option("session-id")
        .map(|value| Uuid::parse_str(&value).context("invalid --session-id UUID"))
        .transpose()?;
    let capture_input = !args.has_flag("no-capture-input");
    let name = normalize_recording_name(args.option("name").as_deref())?;
    let profile_arg = args.option("profile");
    let profile = profile_arg.as_deref().map(parse_profile).transpose()?;
    let event_kinds_raw = args.multi_options("kind");
    let event_kinds_raw = if event_kinds_raw.is_empty() {
        args.multi_options("event-kind")
    } else {
        event_kinds_raw
    };
    let event_kinds = if event_kinds_raw.is_empty() {
        profile.map(|profile| default_event_kinds_for_profile(profile, capture_input))
    } else {
        Some(
            event_kinds_raw
                .iter()
                .map(|value| parse_event_kind(value).map(Into::into))
                .collect::<Result<Vec<_>>>()?,
        )
    };
    let response: std::result::Result<
        recording_types::RecordingSummary,
        recording_types::RecordingError,
    > = invoke_recording_service(
        context,
        "bmux.recording.write",
        InvokeServiceKind::Command,
        "start",
        &StartRequest {
            session_id,
            capture_input,
            name,
            profile,
            event_kinds,
        },
    )?;
    let summary: RecordingSummary = response.map_err(recording_plugin_error)?.into();
    println!(
        "recording started: {} name={} (capture_input={} profile={:?} kinds={})",
        summary.id,
        summary.name.as_deref().unwrap_or("-"),
        summary.capture_input,
        summary.profile,
        summary
            .event_kinds
            .iter()
            .map(|kind| recording_event_kind_name(*kind))
            .collect::<Vec<_>>()
            .join(",")
    );
    Ok(0)
}

fn run_stop(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let recording_id = args
        .positional()
        .first()
        .map(|value| Uuid::parse_str(value).context("invalid recording id"))
        .transpose()?;
    let response: std::result::Result<Uuid, recording_types::RecordingError> =
        invoke_recording_service(
            context,
            "bmux.recording.write",
            InvokeServiceKind::Command,
            "stop",
            &StopRequest { recording_id },
        )?;
    let stopped_id = response.map_err(recording_plugin_error)?;
    println!("recording stopped: {stopped_id}");
    maybe_auto_export_stopped_recording(context, stopped_id);
    Ok(0)
}

fn maybe_auto_export_stopped_recording(context: &NativeCommandContext, recording_id: Uuid) {
    let settings = current_auto_export_settings(None);
    if !settings.enabled {
        return;
    }
    let Ok(root) = recordings_root_for_command(context) else {
        return;
    };
    let recording_dir = root.join(recording_id.to_string());
    let output_path =
        auto_export_output_path(&recording_dir, settings.output_dir.as_deref(), recording_id);
    let output = output_path.display().to_string();
    publish_recording_export_started(recording_id, output.clone());
    match export::export_recording_gif_from_root(
        &root,
        &recording_id.to_string(),
        &output,
        settings.fps,
    ) {
        Ok(()) => {
            publish_recording_export_completed(recording_id, output.clone());
            println!("recording GIF exported: {output}");
        }
        Err(error) => {
            let reason = error.to_string();
            publish_recording_export_failed(recording_id, output.clone(), reason.clone());
            eprintln!("bmux warning: recording auto-export failed: {reason}");
        }
    }
}

fn run_status(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let json = args.has_flag("json");
    let runtime_status: RecordingStatus =
        match invoke_recording_service::<_, recording_types::RecordingStatus>(
            context,
            "bmux.recording.read",
            InvokeServiceKind::Query,
            "status",
            &(),
        ) {
            Ok(status) => status.into(),
            Err(_) => RecordingStatus {
                active: None,
                queue_len: 0,
            },
        };
    let root = recordings_root_for_command(context)?;
    let usage = collect_recording_storage_usage(&root)?;
    if json {
        let payload = serde_json::json!({
            "active": runtime_status.active,
            "queue_len": runtime_status.queue_len,
            "root_path": root,
            "usage": usage,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(0);
    }
    println!("recordings root: {}", root.display());
    if let Some(active) = runtime_status.active {
        println!(
            "active recording: {} name={} events={} bytes={} capture_input={} profile={:?} kinds={} path={}",
            active.id,
            active.name.as_deref().unwrap_or("-"),
            active.event_count,
            active.payload_bytes,
            active.capture_input,
            active.profile,
            active
                .event_kinds
                .iter()
                .map(|kind| recording_event_kind_name(*kind))
                .collect::<Vec<_>>()
                .join(","),
            active.path
        );
    } else {
        println!("active recording: none");
    }
    println!("queue length: {}", runtime_status.queue_len);
    println!(
        "usage: bytes={} ({}) files={} dirs={} recordings={}",
        usage.bytes,
        format_byte_size(usage.bytes),
        usage.files,
        usage.directories,
        usage.recording_dirs
    );
    Ok(0)
}

fn run_path(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let root = recordings_root_for_command(context)?;
    if args.has_flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "path": root }))?
        );
    } else {
        println!("{}", root.display());
    }
    Ok(0)
}

fn run_playbook_from_recording(
    context: &NativeCommandContext,
    args: &ArgCursor<'_>,
) -> Result<i32> {
    let positional = args.positional();
    let Some(recording_id) = positional.first() else {
        anyhow::bail!("playbook from-recording requires a recording id/name or unique prefix");
    };
    let events = load_recording_events(context, recording_id)?;
    let playbook_dsl = crate::from_recording::events_to_playbook(&events);
    if let Some(output) = args.option("output").or_else(|| args.short_option("o")) {
        std::fs::write(&output, playbook_dsl)
            .with_context(|| format!("failed writing playbook to {output}"))?;
        println!("wrote playbook to {output}");
    } else {
        print!("{playbook_dsl}");
    }
    Ok(0)
}

fn run_server_recording(context: &NativeCommandContext) -> Result<i32> {
    let Some(subcommand) = context.arguments.first().map(String::as_str) else {
        anyhow::bail!("server recording requires a subcommand: start, stop, status, path, clear");
    };
    let args = ArgCursor::new(&context.arguments[1..]);
    match subcommand {
        "start" => run_server_recording_start(context, &args),
        "stop" => run_server_recording_stop(context),
        "status" => run_server_recording_status(context, &args),
        "path" => run_server_recording_path(context, &args),
        "clear" => run_server_recording_clear(context, &args),
        other => anyhow::bail!("unknown server recording subcommand '{other}'"),
    }
}

fn run_server_recording_start(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let event_kinds_raw = args.multi_options("rolling-event-kind");
    let event_kinds = if args.has_flag("rolling-event-kind-all") {
        Some(
            all_recording_event_kinds()
                .into_iter()
                .map(Into::into)
                .collect(),
        )
    } else if event_kinds_raw.is_empty() {
        None
    } else {
        Some(
            event_kinds_raw
                .iter()
                .map(|value| parse_event_kind(value).map(Into::into))
                .collect::<Result<Vec<_>>>()?,
        )
    };
    let options = recording_types::RecordingRollingStartOptions {
        window_secs: args.u64_option("rolling-window-secs")?,
        name: normalize_recording_name(args.option("name").as_deref())?,
        event_kinds,
        capture_input: bool_flag_override(
            args.has_flag("rolling-capture-input"),
            args.has_flag("no-rolling-capture-input"),
            "rolling-capture-input",
        )?,
        capture_output: bool_flag_override(
            args.has_flag("rolling-capture-output"),
            args.has_flag("no-rolling-capture-output"),
            "rolling-capture-output",
        )?,
        capture_events: bool_flag_override(
            args.has_flag("rolling-capture-events"),
            args.has_flag("no-rolling-capture-events"),
            "rolling-capture-events",
        )?,
        capture_protocol_replies: bool_flag_override(
            args.has_flag("rolling-capture-protocol-replies"),
            args.has_flag("no-rolling-capture-protocol-replies"),
            "rolling-capture-protocol-replies",
        )?,
        capture_images: bool_flag_override(
            args.has_flag("rolling-capture-images"),
            args.has_flag("no-rolling-capture-images"),
            "rolling-capture-images",
        )?,
    };
    let response: std::result::Result<
        recording_types::RecordingSummary,
        recording_types::RecordingError,
    > = invoke_recording_service(
        context,
        "bmux.recording.write",
        InvokeServiceKind::Command,
        "rolling-start",
        &options,
    )?;
    let recording: RecordingSummary = response.map_err(recording_plugin_error)?.into();
    println!(
        "server rolling recording started: {} name={} path={}",
        recording.id,
        recording.name.as_deref().unwrap_or("-"),
        recording.path
    );
    Ok(0)
}

fn run_server_recording_stop(context: &NativeCommandContext) -> Result<i32> {
    let response: std::result::Result<Uuid, recording_types::RecordingError> =
        invoke_recording_service(
            context,
            "bmux.recording.write",
            InvokeServiceKind::Command,
            "rolling-stop",
            &(),
        )?;
    let recording_id = response.map_err(recording_plugin_error)?;
    println!("server rolling recording stopped: {recording_id}");
    Ok(0)
}

fn fetch_server_recording_status(context: &NativeCommandContext) -> Result<RecordingRollingStatus> {
    let status: recording_types::RecordingRollingStatus = invoke_recording_service(
        context,
        "bmux.recording.read",
        InvokeServiceKind::Query,
        "rolling-status",
        &(),
    )?;
    Ok(status.into())
}

fn run_server_recording_status(
    context: &NativeCommandContext,
    args: &ArgCursor<'_>,
) -> Result<i32> {
    let status = fetch_server_recording_status(context)?;
    if args.has_flag("json") {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(0);
    }

    println!("rolling root: {}", status.root_path);
    println!(
        "auto-start: {}",
        if status.auto_start {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "configured: {}",
        if status.available { "yes" } else { "no" }
    );
    match status.rolling_window_secs {
        Some(window_secs) => println!("window seconds: {window_secs}"),
        None => println!("window seconds: unset"),
    }
    if status.event_kinds.is_empty() {
        println!("event kinds: none");
    } else {
        println!(
            "event kinds: {}",
            status
                .event_kinds
                .iter()
                .map(|kind| recording_event_kind_name(*kind))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(active) = status.active {
        println!(
            "active: {} name={} events={} bytes={} ({}) path={}",
            active.id,
            active.name.as_deref().unwrap_or("-"),
            active.event_count,
            active.payload_bytes,
            format_byte_size(active.payload_bytes),
            active.path
        );
    } else {
        println!("active: none");
    }
    println!(
        "usage: bytes={} ({}) files={} dirs={} recordings={}",
        status.usage.bytes,
        format_byte_size(status.usage.bytes),
        status.usage.files,
        status.usage.directories,
        status.usage.recording_dirs
    );
    Ok(0)
}

fn run_server_recording_path(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let status = fetch_server_recording_status(context)?;
    if args.has_flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "path": status.root_path }))?
        );
    } else {
        println!("{}", status.root_path);
    }
    Ok(0)
}

fn run_server_recording_clear(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let restart_if_active = !args.has_flag("no-restart");
    let report: RecordingRollingClearReport =
        invoke_recording_service::<_, recording_types::RecordingRollingClearReport>(
            context,
            "bmux.recording.write",
            InvokeServiceKind::Command,
            "rolling-clear",
            &restart_if_active,
        )?
        .into();

    if args.has_flag("json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }

    println!("rolling root: {}", report.root_path);
    println!(
        "usage before: bytes={} ({}) files={} dirs={} recordings={}",
        report.usage_before.bytes,
        format_byte_size(report.usage_before.bytes),
        report.usage_before.files,
        report.usage_before.directories,
        report.usage_before.recording_dirs
    );
    println!(
        "usage after: bytes={} ({}) files={} dirs={} recordings={}",
        report.usage_after.bytes,
        format_byte_size(report.usage_after.bytes),
        report.usage_after.files,
        report.usage_after.directories,
        report.usage_after.recording_dirs
    );
    if report.was_active {
        println!("was active: yes");
        if let Some(recording_id) = report.stopped_recording_id {
            println!("stopped recording: {recording_id}");
        }
    } else {
        println!("was active: no");
    }
    if report.restarted {
        if let Some(recording) = report.restarted_recording {
            println!(
                "restarted: yes id={} name={} path={}",
                recording.id,
                recording.name.as_deref().unwrap_or("-"),
                recording.path
            );
        } else {
            println!("restarted: yes");
        }
    } else {
        println!("restarted: no");
    }
    Ok(0)
}

fn bool_flag_override(enabled: bool, disabled: bool, name: &str) -> Result<Option<bool>> {
    if enabled && disabled {
        anyhow::bail!("--{name} conflicts with --no-{name}");
    }
    Ok(if enabled {
        Some(true)
    } else if disabled {
        Some(false)
    } else {
        None
    })
}

fn run_list(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let json = args.has_flag("json");
    let sort = args
        .option("sort")
        .as_deref()
        .map(parse_list_sort)
        .transpose()?
        .unwrap_or(ListSort::Started);
    let order = args
        .option("order")
        .as_deref()
        .map(parse_list_order)
        .transpose()?
        .unwrap_or(default_list_order(sort));
    let status = args
        .option("status")
        .as_deref()
        .map(parse_list_status)
        .transpose()?
        .unwrap_or(ListStatus::All);
    let limit = args.usize_option("limit")?;
    let all = args.has_flag("all");
    let query = args.option("query");
    let mut recordings: Vec<RecordingSummary> =
        match invoke_recording_service::<_, Vec<recording_types::RecordingSummary>>(
            context,
            "bmux.recording.read",
            InvokeServiceKind::Query,
            "list-recordings",
            &(),
        ) {
            Ok(recordings) => recordings.into_iter().map(Into::into).collect(),
            Err(_) => list_recordings_from_dir(&recordings_root_for_command(context)?)?,
        };
    recordings = filter_recordings(recordings, status, query.as_deref());
    sort_recordings(&mut recordings, sort, order);
    let total_count = recordings.len();
    if !all {
        recordings.truncate(limit.unwrap_or(if json { usize::MAX } else { 50 }));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&recordings)?);
        return Ok(0);
    }
    if recordings.is_empty() {
        println!("no recordings");
        return Ok(0);
    }
    println!(
        "{:<36} {:<18} {:<8} {:>8} {:>10}",
        "ID", "NAME", "STATUS", "EVENTS", "SIZE"
    );
    for recording in &recordings {
        println!(
            "{:<36} {:<18} {:<8} {:>8} {:>10}",
            recording.id,
            truncate_for_table(recording.name.as_deref().unwrap_or("-"), 18),
            recording_status_label(recording.ended_epoch_ms),
            recording.event_count,
            format_byte_size(recording.payload_bytes)
        );
    }
    if total_count > recordings.len() {
        println!(
            "showing {} of {} recordings (use --limit N or --all)",
            recordings.len(),
            total_count
        );
    }
    Ok(0)
}

fn run_delete(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let Some(raw) = args.positional().first().copied() else {
        anyhow::bail!("recording delete requires a recording id/name");
    };
    let recordings = list_or_query_recordings(context)?;
    let resolved = resolve_recording_id_prefix(raw, &recordings)?;
    if let Ok(response) =
        invoke_recording_service::<_, std::result::Result<Uuid, recording_types::RecordingError>>(
            context,
            "bmux.recording.write",
            InvokeServiceKind::Command,
            "delete",
            &DeleteRequest {
                recording_id: resolved,
            },
        )
    {
        let deleted = response.map_err(recording_plugin_error)?;
        println!("deleted recording {deleted}");
    } else {
        delete_recording_dir_at(&recordings_root_for_command(context)?, resolved)?;
        println!("deleted recording {resolved}");
    }
    Ok(0)
}

fn run_delete_all(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    if !args.has_flag("yes") && !confirm_delete_all_recordings()? {
        println!("skipped recording delete-all");
        return Ok(0);
    }
    let deleted_count = match invoke_recording_service::<_, u64>(
        context,
        "bmux.recording.write",
        InvokeServiceKind::Command,
        "delete-all",
        &(),
    ) {
        Ok(count) => usize::try_from(count).unwrap_or(usize::MAX),
        Err(_) => delete_all_recordings_from_dir(&recordings_root_for_command(context)?)?,
    };
    println!("deleted {deleted_count} recordings");
    Ok(0)
}

fn run_cut(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let last_seconds = args.u64_option("last-seconds")?;
    let export_fps = args.u32_option("export-fps")?;
    let name = normalize_recording_name(args.option("name").as_deref())?;
    let completed_job =
        queue_cut_generated_and_wait(context.host_kernel_bridge, last_seconds, name, export_fps)?;
    let recording_path = completed_job.recording_path.as_deref().unwrap_or("-");
    let export_path = completed_job.export_output_path.as_deref();
    match completed_job.status {
        recording_types::RecordingJobStatus::Completed => {
            if let Some(output) = export_path {
                println!("recording GIF exported: {output}");
            }
            println!("recording cut created: {recording_path}");
            Ok(0)
        }
        recording_types::RecordingJobStatus::Failed if completed_job.recording_id.is_some() => {
            let reason = completed_job
                .error
                .as_deref()
                .unwrap_or("recording export failed");
            eprintln!("bmux warning: recording cut created but export failed: {reason}");
            println!("recording cut created: {recording_path}");
            Ok(0)
        }
        recording_types::RecordingJobStatus::Failed => {
            let reason = completed_job
                .error
                .unwrap_or_else(|| "recording cut failed".to_string());
            Err(anyhow::anyhow!(reason))
        }
        other => Err(anyhow::anyhow!(
            "recording cut job ended unexpectedly with status {other:?}"
        )),
    }
}

fn queue_cut_generated_and_wait(
    bridge: Option<HostKernelBridge>,
    last_seconds: Option<u64>,
    name: Option<String>,
    export_fps: Option<u32>,
) -> Result<recording_types::RecordingJob> {
    let worker = std::thread::spawn(move || -> Result<recording_types::RecordingJob> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .context("failed creating recording command async runtime")?;
        runtime.block_on(async move {
            let mut client = HostBridgeTypedClient { bridge };
            let mut last_job =
                recording_commands::client::queue_cut(&mut client, last_seconds, name, export_fps)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("recording queue-cut dispatch failed: {error}")
                    })?;
            println!("recording cut queued: {}", last_job.id);
            loop {
                if matches!(
                    last_job.status,
                    recording_types::RecordingJobStatus::Completed
                        | recording_types::RecordingJobStatus::Failed
                ) {
                    return Ok(last_job);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let job_id = last_job.id;
                last_job = recording_state::client::job_status(&mut client, job_id)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("recording job-status dispatch failed: {error}")
                    })?
                    .ok_or_else(|| anyhow::anyhow!("recording job disappeared: {job_id}"))?;
            }
        })
    });
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("recording cut worker thread panicked"))?
}

fn run_prune(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let older_than = args
        .u64_option("older-than")?
        .or(args.u64_option("older-than-days")?);
    let json = args.has_flag("json");
    let deleted_count = match invoke_recording_service::<_, u64>(
        context,
        "bmux.recording.write",
        InvokeServiceKind::Command,
        "prune",
        &PruneRequest {
            older_than_days: older_than,
        },
    ) {
        Ok(count) => usize::try_from(count).unwrap_or(usize::MAX),
        Err(_) => crate::prune_old_recordings(
            &recordings_root_for_command(context)?,
            older_than.unwrap_or(30),
        )?,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "deleted_count": deleted_count,
                "older_than_days": older_than,
            }))?
        );
    } else if deleted_count > 0 {
        println!("pruned {deleted_count} recording(s)");
    } else {
        println!("no recordings to prune");
    }
    Ok(0)
}

fn run_inspect(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let Some(recording_id) = args.positional().first().copied() else {
        anyhow::bail!("recording inspect requires a recording id/name");
    };
    let limit = args.usize_option("limit")?.unwrap_or(20).max(1);
    let kind = args.option("kind").map(|value| value.to_ascii_lowercase());
    let json = args.has_flag("json");
    let events = load_recording_events(context, recording_id)?;
    let filtered = events
        .into_iter()
        .filter(|event| {
            kind.as_ref()
                .is_none_or(|kind| recording_event_kind_name(event.kind) == *kind)
        })
        .take(limit)
        .collect::<Vec<_>>();
    if json {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(0);
    }
    for event in filtered {
        println!(
            "seq={} t={} kind={:?} session={:?} pane={:?} client={:?}",
            event.seq, event.mono_ns, event.kind, event.session_id, event.pane_id, event.client_id
        );
    }
    Ok(0)
}

fn run_analyze(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let Some(recording_id) = args.positional().first().copied() else {
        anyhow::bail!("recording analyze requires a recording id/name");
    };
    if !args.has_flag("perf") {
        anyhow::bail!("recording analyze currently supports only --perf");
    }
    let json = args.has_flag("json");
    let summary = resolve_recording_summary(context, recording_id)?;
    let events = load_recording_events(context, recording_id)?;
    let report = analyze_perf_events(&events, event_kinds_include_custom(&summary.event_kinds));
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_perf_analysis_text(&report);
    }
    Ok(0)
}

fn run_replay(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let Some(recording_id) = args.positional().first().copied() else {
        anyhow::bail!("recording replay requires a recording id/name");
    };
    let mode = args
        .option("mode")
        .as_deref()
        .map(parse_replay_mode)
        .transpose()?
        .unwrap_or(ReplayMode::Watch);
    let speed = normalize_replay_speed(args.f64_option("speed")?.unwrap_or(1.0));
    let events = load_recording_events(context, recording_id)?;
    match mode {
        ReplayMode::Watch => replay_watch(&events, speed),
        ReplayMode::Interactive => replay_interactive(&events, speed),
        ReplayMode::Verify => {
            let report = verify_recording_report(
                context,
                &events,
                args.option("compare-recording").as_deref(),
                args.option("ignore").as_deref(),
            )?;
            if report.pass {
                println!("verify PASS: {}", report.reason);
                Ok(0)
            } else {
                println!("verify FAIL: {}", report.reason);
                Ok(1)
            }
        }
    }
}

fn run_verify_smoke(context: &NativeCommandContext, args: &ArgCursor<'_>) -> Result<i32> {
    let Some(recording_id) = args.positional().first().copied() else {
        anyhow::bail!("recording verify-smoke requires a recording id/name");
    };
    let events = load_recording_events(context, recording_id)?;
    let report = verify_recording_report(
        context,
        &events,
        args.option("compare-recording").as_deref(),
        args.option("ignore").as_deref(),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(i32::from(!report.pass))
}

fn recording_plugin_error(error: recording_types::RecordingError) -> anyhow::Error {
    match error {
        recording_types::RecordingError::NoActive => anyhow::anyhow!("no active recording"),
        recording_types::RecordingError::Unavailable => {
            anyhow::anyhow!("recording runtime unavailable")
        }
        recording_types::RecordingError::Failed { reason } => anyhow::anyhow!(reason),
    }
}

fn parse_profile(value: &str) -> Result<recording_types::RecordingProfile> {
    match value {
        "full" => Ok(recording_types::RecordingProfile::Full),
        "functional" => Ok(recording_types::RecordingProfile::Functional),
        "visual" => Ok(recording_types::RecordingProfile::Visual),
        _ => anyhow::bail!("unsupported recording profile '{value}'"),
    }
}

fn parse_event_kind(value: &str) -> Result<RecordingEventKind> {
    match value.replace('-', "_").as_str() {
        "pane_input_raw" => Ok(RecordingEventKind::PaneInputRaw),
        "pane_output_raw" => Ok(RecordingEventKind::PaneOutputRaw),
        "protocol_reply_raw" => Ok(RecordingEventKind::ProtocolReplyRaw),
        "pane_image" => Ok(RecordingEventKind::PaneImage),
        "server_event" => Ok(RecordingEventKind::ServerEvent),
        "request_start" => Ok(RecordingEventKind::RequestStart),
        "request_done" => Ok(RecordingEventKind::RequestDone),
        "request_error" => Ok(RecordingEventKind::RequestError),
        "custom" => Ok(RecordingEventKind::Custom),
        _ => anyhow::bail!("unsupported recording event kind '{value}'"),
    }
}

fn all_recording_event_kinds() -> Vec<RecordingEventKind> {
    vec![
        RecordingEventKind::PaneInputRaw,
        RecordingEventKind::PaneOutputRaw,
        RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKind::PaneImage,
        RecordingEventKind::ServerEvent,
        RecordingEventKind::RequestStart,
        RecordingEventKind::RequestDone,
        RecordingEventKind::RequestError,
        RecordingEventKind::Custom,
    ]
}

fn default_event_kinds_for_profile(
    profile: recording_types::RecordingProfile,
    capture_input: bool,
) -> Vec<recording_types::RecordingEventKind> {
    let mut kinds = match profile {
        recording_types::RecordingProfile::Full => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ProtocolReplyRaw,
            RecordingEventKind::PaneImage,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        recording_types::RecordingProfile::Functional => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::PaneImage,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        recording_types::RecordingProfile::Visual => vec![RecordingEventKind::PaneOutputRaw],
    };
    if capture_input && profile != recording_types::RecordingProfile::Visual {
        kinds.push(RecordingEventKind::PaneInputRaw);
    }
    kinds.into_iter().map(Into::into).collect()
}

fn normalize_recording_name(name: Option<&str>) -> Result<Option<String>> {
    name.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.len() > 128 {
                anyhow::bail!("recording name must be at most 128 bytes");
            }
            Ok(value.to_string())
        })
        .transpose()
}

#[derive(Debug, serde::Serialize)]
struct RecordingStorageUsage {
    bytes: u64,
    files: u64,
    directories: u64,
    recording_dirs: u64,
}

fn collect_recording_storage_usage(root: &Path) -> Result<RecordingStorageUsage> {
    let mut usage = RecordingStorageUsage {
        bytes: 0,
        files: 0,
        directories: 0,
        recording_dirs: 0,
    };
    if !root.exists() {
        return Ok(usage);
    }
    collect_recording_storage_usage_recursive(root, true, &mut usage)?;
    Ok(usage)
}

fn collect_recording_storage_usage_recursive(
    path: &Path,
    is_root: bool,
    usage: &mut RecordingStorageUsage,
) -> Result<()> {
    for entry in
        std::fs::read_dir(path).with_context(|| format!("failed reading {}", path.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if is_root && entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            usage.directories = usage.directories.saturating_add(1);
            if entry.path().join("manifest.json").exists() {
                usage.recording_dirs = usage.recording_dirs.saturating_add(1);
            }
            collect_recording_storage_usage_recursive(&entry.path(), false, usage)?;
        } else if file_type.is_file() {
            usage.files = usage.files.saturating_add(1);
            usage.bytes = usage
                .bytes
                .saturating_add(entry.metadata().map_or(0, |m| m.len()));
        }
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)] // Human-readable byte formatting tolerates rounded f64 units.
fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

const fn recording_status_label(ended_epoch_ms: Option<u64>) -> &'static str {
    if ended_epoch_ms.is_some() {
        "done"
    } else {
        "active"
    }
}

fn truncate_for_table(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{}…", truncated.trim_end())
    } else {
        truncated
    }
}

fn parse_list_sort(value: &str) -> Result<ListSort> {
    match value {
        "started" => Ok(ListSort::Started),
        "name" => Ok(ListSort::Name),
        "events" => Ok(ListSort::Events),
        "size" => Ok(ListSort::Size),
        _ => anyhow::bail!("unsupported recording list sort '{value}'"),
    }
}

fn parse_list_order(value: &str) -> Result<ListOrder> {
    match value {
        "asc" => Ok(ListOrder::Asc),
        "desc" => Ok(ListOrder::Desc),
        _ => anyhow::bail!("unsupported recording list order '{value}'"),
    }
}

fn parse_list_status(value: &str) -> Result<ListStatus> {
    match value {
        "all" => Ok(ListStatus::All),
        "active" => Ok(ListStatus::Active),
        "done" => Ok(ListStatus::Done),
        _ => anyhow::bail!("unsupported recording list status '{value}'"),
    }
}

const fn default_list_order(sort: ListSort) -> ListOrder {
    match sort {
        ListSort::Started => ListOrder::Desc,
        ListSort::Name | ListSort::Events | ListSort::Size => ListOrder::Asc,
    }
}

fn filter_recordings(
    recordings: Vec<RecordingSummary>,
    status: ListStatus,
    query: Option<&str>,
) -> Vec<RecordingSummary> {
    recordings
        .into_iter()
        .filter(|recording| match status {
            ListStatus::All => true,
            ListStatus::Active => recording.ended_epoch_ms.is_none(),
            ListStatus::Done => recording.ended_epoch_ms.is_some(),
        })
        .filter(|recording| query.is_none_or(|query| recording_matches_query(recording, query)))
        .collect()
}

fn recording_matches_query(recording: &RecordingSummary, query: &str) -> bool {
    let query = query.to_ascii_lowercase();
    recording.id.to_string().starts_with(&query)
        || recording
            .name
            .as_ref()
            .is_some_and(|name| name.to_ascii_lowercase().contains(&query))
}

fn sort_recordings(recordings: &mut [RecordingSummary], sort: ListSort, order: ListOrder) {
    match sort {
        ListSort::Started => recordings.sort_by_key(|recording| recording.started_epoch_ms),
        ListSort::Name => recordings.sort_by(|left, right| left.name.cmp(&right.name)),
        ListSort::Events => recordings.sort_by_key(|recording| recording.event_count),
        ListSort::Size => recordings.sort_by_key(|recording| recording.payload_bytes),
    }
    if order == ListOrder::Desc {
        recordings.reverse();
    }
}

#[derive(Debug, serde::Deserialize)]
struct RecordingManifest {
    summary: RecordingSummary,
}

fn read_recording_manifest(manifest_path: &Path) -> Result<RecordingSummary> {
    let bytes = std::fs::read(manifest_path).with_context(|| {
        format!(
            "failed reading recording manifest {}",
            manifest_path.display()
        )
    })?;
    let manifest: RecordingManifest = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed parsing recording manifest {}",
            manifest_path.display()
        )
    })?;
    Ok(manifest.summary)
}

fn list_recordings_from_dir(recordings_root: &Path) -> Result<Vec<RecordingSummary>> {
    if !recordings_root.exists() {
        return Ok(Vec::new());
    }
    let mut recordings = Vec::new();
    for entry in std::fs::read_dir(recordings_root).with_context(|| {
        format!(
            "failed reading recordings dir {}",
            recordings_root.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        if manifest_path.exists()
            && let Ok(summary) = read_recording_manifest(&manifest_path)
        {
            recordings.push(summary);
        }
    }
    recordings.sort_by_key(|recording| Reverse(recording.started_epoch_ms));
    Ok(recordings)
}

fn list_or_query_recordings(context: &NativeCommandContext) -> Result<Vec<RecordingSummary>> {
    match invoke_recording_service::<_, Vec<recording_types::RecordingSummary>>(
        context,
        "bmux.recording.read",
        InvokeServiceKind::Query,
        "list-recordings",
        &(),
    ) {
        Ok(recordings) => Ok(recordings.into_iter().map(Into::into).collect()),
        Err(_) => list_recordings_from_dir(&recordings_root_for_command(context)?),
    }
}

fn resolve_recording_summary(
    context: &NativeCommandContext,
    recording_id: &str,
) -> Result<RecordingSummary> {
    let recordings = list_or_query_recordings(context)?;
    let id = resolve_recording_id_prefix(recording_id, &recordings)?;
    recordings
        .into_iter()
        .find(|recording| recording.id == id)
        .ok_or_else(|| anyhow::anyhow!("recording '{recording_id}' not found after resolving id"))
}

fn load_recording_events(
    context: &NativeCommandContext,
    recording_id: &str,
) -> Result<Vec<RecordingEventEnvelope>> {
    let recordings = list_or_query_recordings(context)?;
    let id = resolve_recording_id_prefix(recording_id, &recordings)?;
    let recording_dir = recordings_root_for_command(context)?.join(id.to_string());
    let manifest_path = recording_dir.join("manifest.json");
    let segments = if manifest_path.exists() {
        let manifest_bytes = std::fs::read(&manifest_path)
            .with_context(|| format!("failed reading manifest {}", manifest_path.display()))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        manifest["summary"]["segments"].as_array().map_or_else(
            || vec!["events_0.bin".to_string()],
            |arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            },
        )
    } else {
        vec!["events.bin".to_string()]
    };
    let mut all_frames = Vec::new();
    for segment_name in &segments {
        let segment_path = recording_dir.join(segment_name);
        if !segment_path.exists() {
            continue;
        }
        let bytes = std::fs::read(&segment_path).with_context(|| {
            format!(
                "failed reading recording segment {}",
                segment_path.display()
            )
        })?;
        let result = read_frames(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "failed parsing recording segment {}: {error}",
                segment_path.display()
            )
        })?;
        all_frames.extend(result.frames);
    }
    Ok(all_frames)
}

fn resolve_recording_id_prefix(value: &str, recordings: &[RecordingSummary]) -> Result<Uuid> {
    let query = value.trim();
    if query.is_empty() {
        anyhow::bail!("recording id/name cannot be empty");
    }
    if let Ok(id) = Uuid::parse_str(query)
        && recordings.iter().any(|recording| recording.id == id)
    {
        return Ok(id);
    }
    let exact_name_matches = recordings
        .iter()
        .filter_map(|recording| {
            recording
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(query))
                .then_some(recording.id)
        })
        .collect::<Vec<_>>();
    match exact_name_matches.as_slice() {
        [id] => return Ok(*id),
        [] => {}
        _ => anyhow::bail!("recording name '{query}' is ambiguous"),
    }
    let normalized = query.to_ascii_lowercase();
    let mut seen = HashSet::new();
    let matches = recordings
        .iter()
        .filter_map(|recording| {
            let id_match = recording.id.to_string().starts_with(&normalized);
            let name_match = recording
                .name
                .as_ref()
                .is_some_and(|name| name.to_ascii_lowercase().starts_with(&normalized));
            (id_match || name_match)
                .then_some(recording.id)
                .filter(|id| seen.insert(*id))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [id] => Ok(*id),
        [] => anyhow::bail!("no recording matches id/name '{value}'"),
        _ => anyhow::bail!("recording id/name '{value}' is ambiguous"),
    }
}

fn delete_recording_dir_at(recordings_root: &Path, recording_id: Uuid) -> Result<()> {
    let dir = recordings_root.join(recording_id.to_string());
    let manifest = dir.join("manifest.json");
    if !manifest.exists() {
        anyhow::bail!("recording not found: {recording_id}");
    }
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("failed removing recording directory {}", dir.display()))?;
    Ok(())
}

fn delete_all_recordings_from_dir(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut deleted_count = 0_usize;
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("failed reading recordings dir {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if !entry.path().join("manifest.json").exists() {
            continue;
        }
        std::fs::remove_dir_all(entry.path())?;
        deleted_count = deleted_count.saturating_add(1);
    }
    Ok(deleted_count)
}

fn confirm_delete_all_recordings() -> Result<bool> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("recording delete-all requires --yes in non-interactive mode");
    }
    println!("Delete all recordings? [y/N]");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let trimmed = answer.trim().to_ascii_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

pub(crate) fn recording_event_kind_name(kind: RecordingEventKind) -> String {
    match kind {
        RecordingEventKind::PaneInputRaw => "pane_input_raw",
        RecordingEventKind::PaneOutputRaw => "pane_output_raw",
        RecordingEventKind::ProtocolReplyRaw => "protocol_reply_raw",
        RecordingEventKind::PaneImage => "pane_image",
        RecordingEventKind::ServerEvent => "server_event",
        RecordingEventKind::RequestStart => "request_start",
        RecordingEventKind::RequestDone => "request_done",
        RecordingEventKind::RequestError => "request_error",
        RecordingEventKind::Custom => "custom",
    }
    .to_string()
}

fn event_kinds_include_custom(kinds: &[RecordingEventKind]) -> bool {
    kinds.contains(&RecordingEventKind::Custom)
}

#[derive(Debug, Clone, serde::Serialize, Default)]
struct PerfAnalysisReport {
    total_events: usize,
    custom_events: usize,
    counters: BTreeMap<String, u64>,
    hints: Vec<String>,
    custom_events_captured: bool,
}

fn analyze_perf_events(
    events: &[RecordingEventEnvelope],
    custom_events_captured: bool,
) -> PerfAnalysisReport {
    let mut counters = BTreeMap::new();
    let mut custom_events = 0usize;
    for event in events {
        if let RecordingPayload::Custom {
            source,
            name,
            payload,
        } = &event.payload
        {
            if source != "bmux.perf" {
                continue;
            }
            custom_events = custom_events.saturating_add(1);
            *counters.entry(name.clone()).or_insert(0) += 1;
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload)
                && let Some(object) = value.as_object()
            {
                for (key, value) in object {
                    if let Some(count) = value.as_u64() {
                        *counters.entry(format!("{name}.{key}")).or_insert(0) += count;
                    }
                }
            }
        }
    }
    let mut hints = Vec::new();
    if !custom_events_captured {
        hints.push(
            "recording event kinds did not include custom; performance telemetry was not captured"
                .to_string(),
        );
    } else if custom_events == 0 {
        hints.push("no bmux.perf custom events were found".to_string());
    }
    PerfAnalysisReport {
        total_events: events.len(),
        custom_events,
        counters,
        hints,
        custom_events_captured,
    }
}

fn print_perf_analysis_text(report: &PerfAnalysisReport) {
    println!("recording perf analysis");
    println!(
        "events: total={} custom={}",
        report.total_events, report.custom_events
    );
    if !report.counters.is_empty() {
        println!("counters:");
        for (key, value) in &report.counters {
            println!("  {key}: {value}");
        }
    }
    if !report.hints.is_empty() {
        println!("hints:");
        for hint in &report.hints {
            println!("  - {hint}");
        }
    }
}

const REPLAY_SPEED_MIN: f64 = 0.125;
const REPLAY_SPEED_MAX: f64 = 32.0;

fn parse_replay_mode(value: &str) -> Result<ReplayMode> {
    match value {
        "watch" => Ok(ReplayMode::Watch),
        "interactive" => Ok(ReplayMode::Interactive),
        "verify" => Ok(ReplayMode::Verify),
        _ => anyhow::bail!("unsupported recording replay mode '{value}'"),
    }
}

fn normalize_replay_speed(speed: f64) -> f64 {
    if !speed.is_finite() || speed <= 0.0 {
        1.0
    } else {
        speed.clamp(REPLAY_SPEED_MIN, REPLAY_SPEED_MAX)
    }
}

fn replay_watch(events: &[RecordingEventEnvelope], speed: f64) -> Result<i32> {
    let speed = normalize_replay_speed(speed);
    let mut stdout = io::stdout();
    let mut last_ns = None;
    for event in events {
        if let Some(previous) = last_ns {
            let delta = event.mono_ns.saturating_sub(previous);
            let sleep = Duration::from_nanos(delta).div_f64(speed);
            if sleep > Duration::from_millis(0) {
                std::thread::sleep(sleep.min(Duration::from_millis(500)));
            }
        }
        if write_replay_event(&mut stdout, event)? {
            stdout.flush()?;
        }
        last_ns = Some(event.mono_ns);
    }
    Ok(0)
}

fn replay_interactive(events: &[RecordingEventEnvelope], speed: f64) -> Result<i32> {
    let mut state_paused = false;
    let speed = normalize_replay_speed(speed);
    let _guard = ReplayRawModeGuard::enter()?;
    let mut stdout = io::stdout();
    let mut last_ns = None;
    for event in events {
        while state_paused {
            match read_interactive_replay_action_blocking()? {
                Some(InteractiveReplayAction::TogglePause) => state_paused = false,
                Some(InteractiveReplayAction::Step) => break,
                Some(InteractiveReplayAction::Quit) => return Ok(0),
                None => {}
            }
        }
        if let Some(previous) = last_ns {
            let delta = event.mono_ns.saturating_sub(previous);
            let wait = Duration::from_nanos(delta)
                .div_f64(speed)
                .min(Duration::from_millis(500));
            let started = std::time::Instant::now();
            while started.elapsed() < wait {
                if let Some(action) =
                    read_interactive_replay_action_timeout(Duration::from_millis(25))?
                {
                    match action {
                        InteractiveReplayAction::TogglePause => state_paused = true,
                        InteractiveReplayAction::Step => break,
                        InteractiveReplayAction::Quit => return Ok(0),
                    }
                    if state_paused {
                        break;
                    }
                }
            }
        }
        if write_replay_event(&mut stdout, event)? {
            stdout.flush()?;
        }
        last_ns = Some(event.mono_ns);
    }
    Ok(0)
}

fn write_replay_event(stdout: &mut impl Write, event: &RecordingEventEnvelope) -> Result<bool> {
    match &event.payload {
        RecordingPayload::Bytes { data }
            if matches!(
                event.kind,
                RecordingEventKind::PaneOutputRaw | RecordingEventKind::ProtocolReplyRaw
            ) =>
        {
            stdout.write_all(data)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveReplayAction {
    TogglePause,
    Step,
    Quit,
}

struct ReplayRawModeGuard;

impl ReplayRawModeGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode for replay")?;
        Ok(Self)
    }
}

impl Drop for ReplayRawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn read_interactive_replay_action_timeout(
    timeout: Duration,
) -> Result<Option<InteractiveReplayAction>> {
    if crossterm::event::poll(timeout)? {
        read_interactive_replay_action_blocking()
    } else {
        Ok(None)
    }
}

fn read_interactive_replay_action_blocking() -> Result<Option<InteractiveReplayAction>> {
    loop {
        let event = crossterm::event::read()?;
        if let Event::Key(key) = event
            && let Some(action) = replay_action_from_key_event(key)
        {
            return Ok(Some(action));
        }
    }
}

fn replay_action_from_key_event(key: KeyEvent) -> Option<InteractiveReplayAction> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    replay_action_from_key(key.code, key.modifiers)
}

fn replay_action_from_key(
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<InteractiveReplayAction> {
    match (code, modifiers) {
        (KeyCode::Char(' '), _) => Some(InteractiveReplayAction::TogglePause),
        (KeyCode::Char('n') | KeyCode::Right, _) => Some(InteractiveReplayAction::Step),
        (KeyCode::Char('q') | KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            Some(InteractiveReplayAction::Quit)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct VerifySmokeReport {
    pass: bool,
    reason: String,
    compare_recording: Option<String>,
    ignored_kinds: Vec<String>,
    expected_output_len: Option<usize>,
    actual_output_len: Option<usize>,
    monotonic_timeline: bool,
}

fn verify_recording_report(
    context: &NativeCommandContext,
    baseline: &[RecordingEventEnvelope],
    compare_recording: Option<&str>,
    ignore: Option<&str>,
) -> Result<VerifySmokeReport> {
    let ignore_rules = parse_ignore_rules(ignore);
    let baseline_filtered = apply_ignore_rules(baseline, &ignore_rules);
    if let Some(other_id) = compare_recording {
        let other = load_recording_events(context, other_id)?;
        let other_filtered = apply_ignore_rules(&other, &ignore_rules);
        let mismatch = baseline_filtered
            .iter()
            .zip(other_filtered.iter())
            .position(|(left, right)| left != right);
        if mismatch.is_some() || baseline_filtered.len() != other_filtered.len() {
            return Ok(VerifySmokeReport {
                pass: false,
                reason: "recordings diverged".to_string(),
                compare_recording: Some(other_id.to_string()),
                ignored_kinds: ignore_rules,
                expected_output_len: Some(baseline_filtered.len()),
                actual_output_len: Some(other_filtered.len()),
                monotonic_timeline: true,
            });
        }
        return Ok(VerifySmokeReport {
            pass: true,
            reason: "recordings are identical".to_string(),
            compare_recording: Some(other_id.to_string()),
            ignored_kinds: ignore_rules,
            expected_output_len: Some(baseline_filtered.len()),
            actual_output_len: Some(other_filtered.len()),
            monotonic_timeline: true,
        });
    }
    let monotonic = baseline_filtered
        .windows(2)
        .all(|pair| pair[1].seq > pair[0].seq && pair[1].mono_ns >= pair[0].mono_ns);
    Ok(VerifySmokeReport {
        pass: monotonic,
        reason: if monotonic {
            "timeline integrity checks succeeded".to_string()
        } else {
            "non-monotonic sequence or timestamp ordering".to_string()
        },
        compare_recording: None,
        ignored_kinds: ignore_rules,
        expected_output_len: Some(expected_output_bytes(&baseline_filtered, None).len()),
        actual_output_len: None,
        monotonic_timeline: monotonic,
    })
}

fn parse_ignore_rules(ignore: Option<&str>) -> Vec<String> {
    ignore
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn apply_ignore_rules(
    events: &[RecordingEventEnvelope],
    ignore_rules: &[String],
) -> Vec<RecordingEventEnvelope> {
    if ignore_rules.is_empty() {
        return events.to_vec();
    }
    events
        .iter()
        .filter(|event| !ignore_rules.contains(&recording_event_kind_name(event.kind)))
        .cloned()
        .collect()
}

fn expected_output_bytes(events: &[RecordingEventEnvelope], min_mono_ns: Option<u64>) -> Vec<u8> {
    let mut output = Vec::new();
    for event in events {
        if let Some(min_mono_ns) = min_mono_ns
            && event.mono_ns < min_mono_ns
        {
            continue;
        }
        if matches!(event.kind, RecordingEventKind::PaneOutputRaw)
            && let RecordingPayload::Bytes { data } = &event.payload
        {
            output.extend_from_slice(data);
        }
    }
    output
}
