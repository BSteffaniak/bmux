//! Interactive playbook session with socket-based agent control.
//!
//! Starts an ephemeral sandbox server, listens on a platform-specific IPC socket,
//! and accepts a single client connection. The client sends DSL command lines and
//! receives JSON result lines back.
//!
//! Protocol:
//! - Agent → bmux: one DSL command line per `\n` (same syntax as batch DSL)
//! - bmux → Agent: one JSON object per `\n`
//! - Special commands: `quit`, `screen`, `status`

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};
use uuid::Uuid;

use super::engine::{drain_output_until_idle, execute_step, start_recording};
use super::parse_dsl::parse_action_line;
use super::sandbox::SandboxServer;
use super::screen::{ScreenDeltaEvent, ScreenDeltaFormat, ScreenInspector};
use super::types::{PaneCapture, SnapshotCapture, Step};

/// Default timeout for sandbox server startup.
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// JSON response sent back to the agent for each command.
#[derive(Serialize)]
struct InteractiveResponse {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    message_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mono_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<SnapshotCapture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    panes: Option<Vec<PaneCapture>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focused_pane: Option<u32>,
    /// For push events: the event type (e.g. `"output"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    event_type: Option<String>,
    /// For output push events: the pane index that produced the output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_index: Option<u32>,
    /// For output push events: the new output text.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_delta: Option<ScreenDeltaEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor_delta: Option<CursorDeltaEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    watchpoint_hit: Option<WatchpointHitEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_input: Option<PaneInputEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_event: Option<ServerEventPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_lifecycle: Option<RequestLifecycleEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_window: Option<Vec<Value>>,
}

#[derive(Serialize)]
struct CursorDeltaEvent {
    pane_index: u32,
    from: CursorPosition,
    to: CursorPosition,
    distance: u16,
}

#[derive(Serialize)]
struct CursorPosition {
    row: u16,
    col: u16,
}

#[derive(Serialize)]
struct WatchpointHitEvent {
    id: String,
    kind: &'static str,
    watch_event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_index: Option<u32>,
    summary: String,
    min_hits: u16,
    observed_hits: u16,
    window_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_distance: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_seq_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_seq_end: Option<u64>,
}

#[derive(Clone, Serialize)]
struct PaneInputEvent {
    pane_index: Option<u32>,
    byte_len: usize,
    printable_preview: String,
}

#[derive(Serialize)]
struct ServerEventPayload {
    name: String,
    payload: Value,
}

#[derive(Serialize)]
struct RequestLifecycleEvent {
    phase: &'static str,
    request_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct StoredMessage {
    seq: u64,
    event_type: Option<String>,
    payload: Value,
}

#[derive(Debug, Default)]
struct EventBuffer {
    messages: std::collections::VecDeque<StoredMessage>,
    max_messages: usize,
    last_watchpoint_hit_seq: std::collections::HashMap<String, u64>,
}

impl EventBuffer {
    fn with_capacity(max_messages: usize) -> Self {
        Self {
            messages: std::collections::VecDeque::new(),
            max_messages,
            last_watchpoint_hit_seq: std::collections::HashMap::new(),
        }
    }

    fn push(&mut self, message: StoredMessage) {
        if message.event_type.as_deref() == Some("watchpoint_hit")
            && let Some(id) = message
                .payload
                .get("watchpoint_hit")
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
        {
            self.last_watchpoint_hit_seq
                .insert(id.to_string(), message.seq);
        }
        self.messages.push_back(message);
        while self.messages.len() > self.max_messages {
            self.messages.pop_front();
        }
    }

    fn window(&self, start_seq: u64, end_seq: u64) -> Vec<Value> {
        self.messages
            .iter()
            .filter(|entry| entry.seq >= start_seq && entry.seq <= end_seq)
            .map(|entry| entry.payload.clone())
            .collect()
    }

    fn around(&self, center_seq: u64, radius: u64) -> Vec<Value> {
        let start = center_seq.saturating_sub(radius);
        let end = center_seq.saturating_add(radius);
        self.window(start, end)
    }

    fn latest_seq(&self) -> u64 {
        self.messages.back().map_or(0, |entry| entry.seq)
    }
}

impl InteractiveResponse {
    fn ok(action: &str) -> Self {
        Self {
            message_type: None,
            seq: None,
            mono_ns: None,
            request_id: None,
            status: "ok",
            action: Some(action.to_string()),
            elapsed_ms: None,
            detail: None,
            error: None,
            snapshot: None,
            panes: None,
            session_id: None,
            pane_count: None,
            focused_pane: None,
            event_type: None,
            pane_index: None,
            output_data: None,
            code: None,
            retryable: None,
            subscription_id: None,
            screen_delta: None,
            cursor_delta: None,
            watchpoint_hit: None,
            pane_input: None,
            server_event: None,
            request_lifecycle: None,
            event_window: None,
        }
    }

    fn ok_with_detail(action: &str, elapsed_ms: u64, detail: Option<String>) -> Self {
        Self {
            message_type: None,
            seq: None,
            mono_ns: None,
            request_id: None,
            status: "ok",
            action: Some(action.to_string()),
            elapsed_ms: Some(elapsed_ms),
            detail,
            error: None,
            snapshot: None,
            panes: None,
            session_id: None,
            pane_count: None,
            focused_pane: None,
            event_type: None,
            pane_index: None,
            output_data: None,
            code: None,
            retryable: None,
            subscription_id: None,
            screen_delta: None,
            cursor_delta: None,
            watchpoint_hit: None,
            pane_input: None,
            server_event: None,
            request_lifecycle: None,
            event_window: None,
        }
    }

    fn fail(action: &str, elapsed_ms: u64, error: String) -> Self {
        Self {
            message_type: None,
            seq: None,
            mono_ns: None,
            request_id: None,
            status: "fail",
            action: Some(action.to_string()),
            elapsed_ms: Some(elapsed_ms),
            detail: None,
            error: Some(error),
            snapshot: None,
            panes: None,
            session_id: None,
            pane_count: None,
            focused_pane: None,
            event_type: None,
            pane_index: None,
            output_data: None,
            code: None,
            retryable: None,
            subscription_id: None,
            screen_delta: None,
            cursor_delta: None,
            watchpoint_hit: None,
            pane_input: None,
            server_event: None,
            request_lifecycle: None,
            event_window: None,
        }
    }

    fn error(message: String) -> Self {
        Self {
            message_type: None,
            seq: None,
            mono_ns: None,
            request_id: None,
            status: "error",
            action: None,
            elapsed_ms: None,
            detail: None,
            error: Some(message),
            snapshot: None,
            panes: None,
            session_id: None,
            pane_count: None,
            focused_pane: None,
            event_type: None,
            pane_index: None,
            output_data: None,
            code: Some("internal".to_string()),
            retryable: Some(false),
            subscription_id: None,
            screen_delta: None,
            cursor_delta: None,
            watchpoint_hit: None,
            pane_input: None,
            server_event: None,
            request_lifecycle: None,
            event_window: None,
        }
    }

    /// Create a push output event.
    fn push_output(pane_index: u32, data: String) -> Self {
        Self {
            message_type: None,
            seq: None,
            mono_ns: None,
            request_id: None,
            status: "ok",
            action: None,
            elapsed_ms: None,
            detail: None,
            error: None,
            snapshot: None,
            panes: None,
            session_id: None,
            pane_count: None,
            focused_pane: None,
            event_type: Some("output".to_string()),
            pane_index: Some(pane_index),
            output_data: Some(data),
            code: None,
            retryable: None,
            subscription_id: None,
            screen_delta: None,
            cursor_delta: None,
            watchpoint_hit: None,
            pane_input: None,
            server_event: None,
            request_lifecycle: None,
            event_window: None,
        }
    }

    fn event_screen_delta(delta: ScreenDeltaEvent) -> Self {
        Self {
            event_type: Some("screen_delta".to_string()),
            screen_delta: Some(delta),
            ..Self::ok("screen-delta")
        }
    }

    fn event_cursor_delta(delta: CursorDeltaEvent) -> Self {
        Self {
            event_type: Some("cursor_delta".to_string()),
            cursor_delta: Some(delta),
            ..Self::ok("cursor-delta")
        }
    }

    fn event_watchpoint_hit(hit: WatchpointHitEvent) -> Self {
        Self {
            event_type: Some("watchpoint_hit".to_string()),
            watchpoint_hit: Some(hit),
            ..Self::ok("watchpoint-hit")
        }
    }

    fn event_pane_input(event: PaneInputEvent) -> Self {
        Self {
            event_type: Some("pane_input".to_string()),
            pane_input: Some(event),
            ..Self::ok("pane-input")
        }
    }

    fn event_server_event(event: ServerEventPayload) -> Self {
        Self {
            event_type: Some("server_event".to_string()),
            server_event: Some(event),
            ..Self::ok("server-event")
        }
    }

    fn event_request_lifecycle(event: RequestLifecycleEvent) -> Self {
        Self {
            event_type: Some("request_lifecycle".to_string()),
            request_lifecycle: Some(event),
            ..Self::ok("request-lifecycle")
        }
    }
}

#[derive(Debug, Deserialize)]
struct InteractiveJsonRequest {
    op: String,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    prefer_machine_readable: Option<bool>,
    #[serde(default)]
    dsl: Option<String>,
    #[serde(default)]
    event_types: Vec<String>,
    #[serde(default)]
    pane_indexes: Vec<u32>,
    #[serde(default)]
    screen_delta_format: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    pane_index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_ms: Option<u64>,
    #[serde(default)]
    min_hits: Option<u16>,
    #[serde(default)]
    event_type: Option<String>,
    #[serde(default)]
    contains_regex: Option<String>,
    #[serde(default)]
    max_events_per_sec: Option<u32>,
    #[serde(default)]
    max_bytes_per_sec: Option<usize>,
    #[serde(default)]
    coalesce_ms: Option<u64>,
    #[serde(default)]
    start_seq: Option<u64>,
    #[serde(default)]
    end_seq: Option<u64>,
    #[serde(default)]
    around_seq: Option<u64>,
    #[serde(default)]
    window_radius: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeltaFormatPreference {
    Auto,
    LineOps,
    UnifiedDiff,
}

#[derive(Debug, Clone)]
struct LiveSubscription {
    active: bool,
    event_types: std::collections::BTreeSet<String>,
    pane_indexes: Option<std::collections::BTreeSet<u32>>,
    format_preference: DeltaFormatPreference,
    prefer_machine_readable: bool,
    subscription_id: String,
    max_events_per_sec: Option<u32>,
    max_bytes_per_sec: Option<usize>,
    coalesce_ms: u64,
}

impl Default for LiveSubscription {
    fn default() -> Self {
        Self {
            active: false,
            event_types: ["pane_output".to_string()].into_iter().collect(),
            pane_indexes: None,
            format_preference: DeltaFormatPreference::Auto,
            prefer_machine_readable: false,
            subscription_id: "sub_1".to_string(),
            max_events_per_sec: Some(500),
            max_bytes_per_sec: Some(256 * 1024),
            coalesce_ms: 0,
        }
    }
}

impl LiveSubscription {
    fn wants(&self, event_type: &str) -> bool {
        self.event_types.contains(event_type)
    }

    fn allows_pane(&self, pane_index: u32) -> bool {
        self.pane_indexes
            .as_ref()
            .is_none_or(|panes| panes.contains(&pane_index))
    }

    fn resolved_delta_format(&self) -> ScreenDeltaFormat {
        match self.format_preference {
            DeltaFormatPreference::LineOps => ScreenDeltaFormat::LineOps,
            DeltaFormatPreference::UnifiedDiff => ScreenDeltaFormat::UnifiedDiff,
            DeltaFormatPreference::Auto => {
                if self.prefer_machine_readable {
                    ScreenDeltaFormat::LineOps
                } else {
                    ScreenDeltaFormat::UnifiedDiff
                }
            }
        }
    }
}

#[derive(Debug)]
struct EventBudgetState {
    window_started: Instant,
    sent_events: u32,
    sent_bytes: usize,
    last_sent_at: std::collections::HashMap<(String, Option<u32>), Instant>,
}

impl EventBudgetState {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            sent_events: 0,
            sent_bytes: 0,
            last_sent_at: std::collections::HashMap::new(),
        }
    }

    fn allows(
        &mut self,
        subscription: &LiveSubscription,
        event_type: &str,
        pane_index: Option<u32>,
        approx_bytes: usize,
    ) -> bool {
        if self.window_started.elapsed() >= Duration::from_secs(1) {
            self.window_started = Instant::now();
            self.sent_events = 0;
            self.sent_bytes = 0;
        }

        if subscription.coalesce_ms > 0 {
            let key = (event_type.to_string(), pane_index);
            if let Some(last) = self.last_sent_at.get(&key)
                && last.elapsed() < Duration::from_millis(subscription.coalesce_ms)
            {
                return false;
            }
            self.last_sent_at.insert(key, Instant::now());
        }

        if let Some(max_events) = subscription.max_events_per_sec
            && self.sent_events >= max_events
        {
            return false;
        }
        if let Some(max_bytes) = subscription.max_bytes_per_sec
            && self.sent_bytes.saturating_add(approx_bytes) > max_bytes
        {
            return false;
        }

        self.sent_events = self.sent_events.saturating_add(1);
        self.sent_bytes = self.sent_bytes.saturating_add(approx_bytes);
        true
    }
}

struct MessageSequencer {
    started: Instant,
    seq: u64,
}

impl MessageSequencer {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            seq: 0,
        }
    }

    fn stamp(
        &mut self,
        response: &mut InteractiveResponse,
        message_type: &str,
        request_id: Option<&str>,
    ) -> u64 {
        self.seq = self.seq.saturating_add(1);
        response.message_type = Some(message_type.to_string());
        response.seq = Some(self.seq);
        response.mono_ns = Some(self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        response.request_id = request_id.map(std::string::ToString::to_string);
        self.seq
    }
}

#[derive(Debug, Clone)]
struct EventBurstWatchpoint {
    id: String,
    event_type: String,
    pane_index: Option<u32>,
    min_hits: u16,
    window_ms: u64,
    contains_regex_raw: Option<String>,
    contains_regex: Option<Regex>,
}

#[derive(Debug, Clone)]
struct EventBurstSample {
    at: Instant,
    peak_distance: Option<u16>,
    seq: Option<u64>,
}

#[derive(Debug, Default)]
struct WatchpointRegistry {
    event_burst: Vec<EventBurstWatchpoint>,
    burst_history:
        std::collections::HashMap<(String, u32), std::collections::VecDeque<EventBurstSample>>,
}

impl WatchpointRegistry {
    fn upsert_event_burst(&mut self, watchpoint: EventBurstWatchpoint) {
        self.event_burst.retain(|entry| entry.id != watchpoint.id);
        self.event_burst.push(watchpoint);
    }

    fn clear(&mut self, id: &str) -> bool {
        let before = self.event_burst.len();
        self.event_burst.retain(|entry| entry.id != id);
        self.burst_history
            .retain(|(watchpoint_id, _), _| watchpoint_id != id);
        before != self.event_burst.len()
    }

    fn is_empty(&self) -> bool {
        self.event_burst.is_empty()
    }
}

/// JSON message printed to stdout when the interactive session is ready.
#[derive(Serialize)]
struct ReadyMessage {
    status: &'static str,
    socket: String,
    sandbox_root: String,
}

/// Entry point for `bmux playbook interactive`.
///
/// Handles Ctrl+C gracefully: on signal, the sandbox server is cleaned up
/// via `SandboxServer`'s `Drop` impl.
pub async fn run_interactive(
    socket_override: Option<&str>,
    record: bool,
    viewport_cols: u16,
    viewport_rows: u16,
    shell: Option<&str>,
    session_timeout: Option<Duration>,
) -> Result<u8> {
    let plugins = super::types::PluginConfig::default();

    // 1. Start sandbox server.
    // Interactive mode has no playbook config, so resolve env mode from
    // BMUX_PLAYBOOK_ENV_MODE env var, falling back to Inherit.
    let env_mode = match std::env::var("BMUX_PLAYBOOK_ENV_MODE").ok().as_deref() {
        Some("clean") => super::types::SandboxEnvMode::Clean,
        _ => super::types::SandboxEnvMode::Inherit,
    };

    let sandbox = SandboxServer::start(
        shell,
        &plugins,
        SERVER_STARTUP_TIMEOUT,
        &std::collections::BTreeMap::new(),
        env_mode,
        None,
        &[],
    )
    .await
    .context("failed starting sandbox server")?;

    // 2. Determine the IPC endpoint.
    let endpoint = interactive_endpoint(socket_override, &sandbox);

    // 3. Run the session with signal handling.
    //    On Ctrl+C, the sandbox is cleaned up via Drop when the select! drops
    //    the inner future (which owns references to the sandbox).
    let result = tokio::select! {
        result = run_interactive_session_managed(&sandbox, &endpoint, record, viewport_cols, viewport_rows, session_timeout) => result,
        _ = tokio::signal::ctrl_c() => {
            info!("interactive session interrupted by signal");
            Ok(130)
        }
    };

    // 4. Cleanup (no-op in Drop if shutdown succeeds).
    if let Err(e) = sandbox.shutdown(false).await {
        warn!("sandbox shutdown error: {e:#}");
    }

    // Clean up socket file if it still exists (Unix only — named pipes don't leave files).
    #[cfg(unix)]
    if let bmux_ipc::IpcEndpoint::UnixSocket(ref path) = endpoint {
        let _ = std::fs::remove_file(path);
    }

    result
}

async fn run_interactive_session_managed(
    sandbox: &SandboxServer,
    endpoint: &bmux_ipc::IpcEndpoint,
    record: bool,
    viewport_cols: u16,
    viewport_rows: u16,
    session_timeout: Option<Duration>,
) -> Result<u8> {
    // Bind the listener using the cross-platform IPC transport.
    let listener = bmux_ipc::transport::LocalIpcListener::bind(endpoint)
        .await
        .with_context(|| format!("failed binding interactive listener on {endpoint:?}"))?;

    // Print ready message to stdout.
    let endpoint_display = match endpoint {
        bmux_ipc::IpcEndpoint::UnixSocket(path) => path.to_string_lossy().to_string(),
        bmux_ipc::IpcEndpoint::WindowsNamedPipe(name) => name.clone(),
    };
    let ready = ReadyMessage {
        status: "ready",
        socket: endpoint_display,
        sandbox_root: sandbox.root_dir().to_string_lossy().to_string(),
    };
    println!("{}", serde_json::to_string(&ready)?);

    // Accept a single client connection with optional timeout.
    let accept_fut = listener.accept();
    let stream = if let Some(timeout_dur) = session_timeout {
        tokio::time::timeout(timeout_dur, accept_fut)
            .await
            .context("timed out waiting for agent connection")?
            .map_err(|e| anyhow::anyhow!("accept failed: {e}"))?
    } else {
        accept_fut
            .await
            .map_err(|e| anyhow::anyhow!("accept failed: {e}"))?
    };
    info!("interactive client connected");

    // Connect to the sandbox server.
    let mut client = sandbox.connect("bmux-playbook-interactive").await?;
    let mut inspector = ScreenInspector::new(viewport_cols, viewport_rows);

    // Session state.
    let mut session_id: Option<Uuid> = None;
    let mut attached = false;
    let mut events_subscribed = false;
    let mut attach_runtime: Option<super::engine::AttachInputRuntime> = None;
    let mut recording_id: Option<Uuid> = None;
    let mut step_counter: usize = 0;
    let mut snapshots: Vec<SnapshotCapture> = Vec::new();
    let mut runtime_vars = super::subst::RuntimeVars::new(std::collections::BTreeMap::new());

    let deadline = session_timeout.map(|d| Instant::now() + d);

    // Run the read-eval-respond loop.
    let loop_result = run_repl(
        stream,
        &mut client,
        &mut inspector,
        &mut session_id,
        &mut attached,
        &mut events_subscribed,
        &mut attach_runtime,
        &mut recording_id,
        &mut step_counter,
        &mut snapshots,
        viewport_cols,
        viewport_rows,
        record,
        deadline,
        &mut runtime_vars,
        sandbox,
    )
    .await;

    // Stop recording if active.
    if let Some(rid) = recording_id {
        match client.recording_stop(Some(rid)).await {
            Ok(stopped) => info!("recording stopped: {stopped}"),
            Err(e) => warn!("failed to stop recording: {e}"),
        }
    }

    match loop_result {
        Ok(()) => Ok(0),
        Err(e) => {
            warn!("interactive session error: {e:#}");
            Ok(1)
        }
    }
}

/// The core read-eval-respond loop.
#[allow(clippy::too_many_arguments)]
async fn run_repl(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    client: &mut bmux_client::BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    events_subscribed: &mut bool,
    attach_runtime: &mut Option<super::engine::AttachInputRuntime>,
    recording_id: &mut Option<Uuid>,
    step_counter: &mut usize,
    snapshots: &mut Vec<SnapshotCapture>,
    viewport_cols: u16,
    viewport_rows: u16,
    record: bool,
    deadline: Option<Instant>,
    runtime_vars: &mut super::subst::RuntimeVars,
    sandbox: &super::sandbox::SandboxServer,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Channel for push output events (populated when subscribed).
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<(u32, String)>(64);
    let (server_event_tx, mut server_event_rx) =
        tokio::sync::mpsc::channel::<bmux_client::ServerEvent>(64);
    let mut output_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut server_event_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut subscription = LiveSubscription::default();
    let mut pane_cache = std::collections::HashMap::<u32, PaneCapture>::new();
    let mut sequencer = MessageSequencer::new();
    let mut event_buffer = EventBuffer::with_capacity(10_000);
    let mut budget_state = EventBudgetState::new();
    let mut watchpoints = WatchpointRegistry::default();

    loop {
        // Check session timeout.
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                let resp = InteractiveResponse::error("session timeout exceeded".to_string());
                send_response(&mut writer, &mut sequencer, resp, "error", None).await?;
                break;
            }
        }

        // Read next command, optionally interleaved with push events.
        line.clear();

        // Drain any pending push events before blocking on the next command.
        if subscription.active {
            while let Ok((pane_idx, data)) = output_rx.try_recv() {
                let mut output_seq = None;
                if subscription.wants("pane_output") && subscription.allows_pane(pane_idx) {
                    let mut push = InteractiveResponse::push_output(pane_idx, data.clone());
                    push.event_type = Some("pane_output".to_string());
                    output_seq = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        push,
                        "pane_output",
                        Some(pane_idx),
                    )
                    .await?;
                }
                if subscription.wants("watchpoint_hit")
                    && !watchpoints.is_empty()
                    && subscription.allows_pane(pane_idx)
                {
                    for hit in evaluate_watchpoints(
                        &mut watchpoints,
                        "pane_output",
                        Some(pane_idx),
                        output_seq,
                        None,
                        Some(&data),
                    ) {
                        let response = InteractiveResponse::event_watchpoint_hit(hit);
                        send_response(&mut writer, &mut sequencer, response, "event", None).await?;
                    }
                }
                emit_screen_events(
                    &mut writer,
                    &mut sequencer,
                    &mut event_buffer,
                    &mut budget_state,
                    client,
                    inspector,
                    session_id,
                    attached,
                    &subscription,
                    &mut pane_cache,
                    &mut watchpoints,
                )
                .await?;
            }
            while let Ok(event) = server_event_rx.try_recv() {
                let mut event_seq = None;
                if subscription.wants("server_event") {
                    let payload = ServerEventPayload {
                        name: server_event_name(&event).to_string(),
                        payload: serde_json::to_value(&event)?,
                    };
                    let response = InteractiveResponse::event_server_event(payload);
                    event_seq = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        response,
                        "server_event",
                        None,
                    )
                    .await?;
                }
                if subscription.wants("watchpoint_hit") && !watchpoints.is_empty() {
                    for hit in evaluate_watchpoints(
                        &mut watchpoints,
                        "server_event",
                        None,
                        event_seq,
                        None,
                        Some(server_event_name(&event)),
                    ) {
                        let response = InteractiveResponse::event_watchpoint_hit(hit);
                        let _ = send_event_if_allowed(
                            &mut writer,
                            &mut sequencer,
                            &mut event_buffer,
                            &subscription,
                            &mut budget_state,
                            response,
                            "watchpoint_hit",
                            None,
                        )
                        .await?;
                    }
                }
            }
        }

        let read_result = if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, reader.read_line(&mut line)).await {
                Ok(result) => result,
                Err(_) => {
                    let resp = InteractiveResponse::error("session timeout exceeded".to_string());
                    send_response(&mut writer, &mut sequencer, resp, "error", None).await?;
                    break;
                }
            }
        } else {
            reader.read_line(&mut line).await
        };

        match read_result {
            Ok(0) => break, // EOF — client disconnected
            Ok(_) => {}
            Err(e) => {
                warn!("read error: {e}");
                break;
            }
        }

        let mut trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        let active_request_id: Option<String>;

        // Drain push events that arrived during the command read.
        if subscription.active {
            while let Ok((pane_idx, data)) = output_rx.try_recv() {
                let mut output_seq = None;
                if subscription.wants("pane_output") && subscription.allows_pane(pane_idx) {
                    let mut push = InteractiveResponse::push_output(pane_idx, data.clone());
                    push.event_type = Some("pane_output".to_string());
                    output_seq = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        push,
                        "pane_output",
                        Some(pane_idx),
                    )
                    .await?;
                }
                if subscription.wants("watchpoint_hit")
                    && !watchpoints.is_empty()
                    && subscription.allows_pane(pane_idx)
                {
                    for hit in evaluate_watchpoints(
                        &mut watchpoints,
                        "pane_output",
                        Some(pane_idx),
                        output_seq,
                        None,
                        Some(&data),
                    ) {
                        let response = InteractiveResponse::event_watchpoint_hit(hit);
                        send_response(&mut writer, &mut sequencer, response, "event", None).await?;
                    }
                }
                emit_screen_events(
                    &mut writer,
                    &mut sequencer,
                    &mut event_buffer,
                    &mut budget_state,
                    client,
                    inspector,
                    session_id,
                    attached,
                    &subscription,
                    &mut pane_cache,
                    &mut watchpoints,
                )
                .await?;
            }
            while let Ok(event) = server_event_rx.try_recv() {
                let mut event_seq = None;
                if subscription.wants("server_event") {
                    let payload = ServerEventPayload {
                        name: server_event_name(&event).to_string(),
                        payload: serde_json::to_value(&event)?,
                    };
                    let response = InteractiveResponse::event_server_event(payload);
                    event_seq = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        response,
                        "server_event",
                        None,
                    )
                    .await?;
                }
                if subscription.wants("watchpoint_hit") && !watchpoints.is_empty() {
                    for hit in evaluate_watchpoints(
                        &mut watchpoints,
                        "server_event",
                        None,
                        event_seq,
                        None,
                        Some(server_event_name(&event)),
                    ) {
                        let response = InteractiveResponse::event_watchpoint_hit(hit);
                        let _ = send_event_if_allowed(
                            &mut writer,
                            &mut sequencer,
                            &mut event_buffer,
                            &subscription,
                            &mut budget_state,
                            response,
                            "watchpoint_hit",
                            None,
                        )
                        .await?;
                    }
                }
            }
        }

        if let Ok(json) = serde_json::from_str::<InteractiveJsonRequest>(&trimmed) {
            if json.op == "command" {
                let Some(dsl) = json.dsl.as_deref() else {
                    let mut resp = InteractiveResponse::error("command requires dsl".to_string());
                    resp.code = Some("invalid_op".to_string());
                    send_response(
                        &mut writer,
                        &mut sequencer,
                        resp,
                        "error",
                        json.request_id.as_deref(),
                    )
                    .await?;
                    continue;
                };
                trimmed = dsl.trim().to_string();
                if trimmed.is_empty() {
                    let mut resp =
                        InteractiveResponse::error("command dsl cannot be empty".to_string());
                    resp.code = Some("invalid_op".to_string());
                    send_response(
                        &mut writer,
                        &mut sequencer,
                        resp,
                        "error",
                        json.request_id.as_deref(),
                    )
                    .await?;
                    continue;
                }
                active_request_id = json.request_id;
            } else {
                let should_continue = handle_json_command(
                    json,
                    &mut writer,
                    &mut sequencer,
                    &mut event_buffer,
                    client,
                    inspector,
                    session_id,
                    attached,
                    &mut subscription,
                    &mut output_task,
                    &mut server_event_task,
                    &output_tx,
                    &server_event_tx,
                    &mut pane_cache,
                    sandbox,
                    &mut watchpoints,
                )
                .await?;
                if !should_continue {
                    break;
                }
                continue;
            }
        } else {
            let mut resp = InteractiveResponse::error(
                "interactive v2 requires JSON operations; plain DSL lines are unsupported"
                    .to_string(),
            );
            resp.code = Some("invalid_op".to_string());
            send_response_buffered(
                &mut writer,
                &mut sequencer,
                &mut event_buffer,
                resp,
                "error",
                None,
            )
            .await?;
            continue;
        }

        // Handle special commands.
        match trimmed.as_str() {
            "quit" => {
                let resp = InteractiveResponse::ok("quit");
                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "response",
                    active_request_id.as_deref(),
                )
                .await?;
                break;
            }
            "screen" => {
                let resp = handle_screen_command(client, inspector, session_id, attached).await;
                update_pane_cache_from_inspector(inspector, &mut pane_cache);
                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "response",
                    active_request_id.as_deref(),
                )
                .await?;
                continue;
            }
            "status" => {
                let resp = handle_status_command(client, inspector, session_id, attached).await;
                update_pane_cache_from_inspector(inspector, &mut pane_cache);
                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "response",
                    active_request_id.as_deref(),
                )
                .await?;
                continue;
            }
            "help" => {
                let resp = InteractiveResponse {
                    status: "ok",
                    action: Some("help".to_string()),
                    detail: Some(
                        "commands: quit, screen, status, help, subscribe, unsubscribe, \
                         or any DSL action (new-session, send-keys, wait-for, \
                         assert-screen, snapshot, etc.)"
                            .to_string(),
                    ),
                    ..InteractiveResponse::ok("help")
                };
                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "response",
                    active_request_id.as_deref(),
                )
                .await?;
                continue;
            }
            "subscribe" => {
                if subscription.active {
                    let resp = InteractiveResponse::ok("subscribe");
                    send_response(
                        &mut writer,
                        &mut sequencer,
                        resp,
                        "response",
                        active_request_id.as_deref(),
                    )
                    .await?;
                    continue;
                }
                let Some(sid) = *session_id else {
                    let resp = InteractiveResponse::error(
                        "no session — use new-session first".to_string(),
                    );
                    send_response(
                        &mut writer,
                        &mut sequencer,
                        resp,
                        "error",
                        active_request_id.as_deref(),
                    )
                    .await?;
                    continue;
                };
                if !*attached {
                    let resp = InteractiveResponse::error("not attached to a session".to_string());
                    send_response(
                        &mut writer,
                        &mut sequencer,
                        resp,
                        "error",
                        active_request_id.as_deref(),
                    )
                    .await?;
                    continue;
                }
                // Create a second client connection for output polling.
                match sandbox.connect("bmux-playbook-output-stream").await {
                    Ok(mut event_client) => {
                        let tx = output_tx.clone();
                        let focused = runtime_vars.focused_pane;
                        output_task = Some(tokio::spawn(async move {
                            output_poll_loop(&mut event_client, sid, focused, tx).await;
                        }));
                        subscription.active = true;
                        subscription.event_types =
                            ["pane_output".to_string()].into_iter().collect();
                        let resp = InteractiveResponse::ok("subscribe");
                        send_response(
                            &mut writer,
                            &mut sequencer,
                            resp,
                            "response",
                            active_request_id.as_deref(),
                        )
                        .await?;
                    }
                    Err(e) => {
                        let resp = InteractiveResponse::error(format!("subscribe failed: {e:#}"));
                        send_response(
                            &mut writer,
                            &mut sequencer,
                            resp,
                            "error",
                            active_request_id.as_deref(),
                        )
                        .await?;
                    }
                }
                continue;
            }
            "unsubscribe" => {
                if let Some(task) = output_task.take() {
                    task.abort();
                }
                subscription.active = false;
                let resp = InteractiveResponse::ok("unsubscribe");
                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "response",
                    active_request_id.as_deref(),
                )
                .await?;
                continue;
            }
            _ => {}
        }

        // Parse as DSL action line.
        let action = match parse_action_line(&trimmed) {
            Ok(action) => action,
            Err(e) => {
                let resp = InteractiveResponse::error(format!("{e:#}"));
                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "error",
                    active_request_id.as_deref(),
                )
                .await?;
                continue;
            }
        };

        let action_name = action.name().to_string();
        let is_new_session = matches!(action, super::types::Action::NewSession { .. });

        let step = Step {
            index: *step_counter,
            action,
            continue_on_error: false,
        };
        *step_counter += 1;

        if subscription.active && subscription.wants("request_lifecycle") {
            let lifecycle = RequestLifecycleEvent {
                phase: "start",
                request_kind: action_name.clone(),
                elapsed_ms: None,
                error: None,
            };
            let response = InteractiveResponse::event_request_lifecycle(lifecycle);
            let lifecycle_seq = send_event_if_allowed(
                &mut writer,
                &mut sequencer,
                &mut event_buffer,
                &subscription,
                &mut budget_state,
                response,
                "request_lifecycle",
                None,
            )
            .await?;
            if subscription.wants("watchpoint_hit") && !watchpoints.is_empty() {
                for hit in evaluate_watchpoints(
                    &mut watchpoints,
                    "request_lifecycle",
                    None,
                    lifecycle_seq,
                    None,
                    Some("start"),
                ) {
                    let response = InteractiveResponse::event_watchpoint_hit(hit);
                    let _ = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        response,
                        "watchpoint_hit",
                        None,
                    )
                    .await?;
                }
            }
        }

        if let Some(input_event) = pane_input_from_action(&step.action) {
            let mut event_seq = None;
            if subscription.active && subscription.wants("pane_input") {
                let response = InteractiveResponse::event_pane_input(input_event.clone());
                event_seq = send_event_if_allowed(
                    &mut writer,
                    &mut sequencer,
                    &mut event_buffer,
                    &subscription,
                    &mut budget_state,
                    response,
                    "pane_input",
                    input_event.pane_index,
                )
                .await?;
            }
            if subscription.active
                && subscription.wants("watchpoint_hit")
                && !watchpoints.is_empty()
            {
                for hit in evaluate_watchpoints(
                    &mut watchpoints,
                    "pane_input",
                    input_event.pane_index,
                    event_seq,
                    None,
                    Some(&input_event.printable_preview),
                ) {
                    let response = InteractiveResponse::event_watchpoint_hit(hit);
                    let _ = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        response,
                        "watchpoint_hit",
                        input_event.pane_index,
                    )
                    .await?;
                }
            }
        }

        // Use a far-future deadline for individual steps if no session timeout.
        let step_deadline = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        let step_start = Instant::now();
        let mut no_display_track: Option<super::display_track::PlaybookDisplayTrackWriter> = None;
        let result = execute_step(
            &step,
            client,
            inspector,
            session_id,
            attached,
            events_subscribed,
            attach_runtime,
            &viewport_cols,
            &viewport_rows,
            snapshots,
            step_deadline,
            &mut no_display_track,
            runtime_vars,
        )
        .await;

        let elapsed_ms = step_start.elapsed().as_millis() as u64;

        match result {
            Ok(detail) => {
                // Start recording after first successful new-session.
                if record && recording_id.is_none() && is_new_session {
                    match start_recording(client, *session_id).await {
                        Ok(rid) => {
                            info!("recording started: {rid}");
                            *recording_id = Some(rid);
                        }
                        Err(e) => warn!("failed to start recording: {e:#}"),
                    }
                }

                let mut resp =
                    InteractiveResponse::ok_with_detail(&action_name, elapsed_ms, detail);

                // For snapshot actions, include the snapshot data in the response.
                if action_name == "snapshot" {
                    if let Some(snap) = snapshots.last() {
                        resp.snapshot = Some(snap.clone());
                    }
                }

                update_pane_cache_from_inspector(inspector, &mut pane_cache);
                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "response",
                    active_request_id.as_deref(),
                )
                .await?;
                if subscription.active && subscription.wants("request_lifecycle") {
                    let lifecycle = RequestLifecycleEvent {
                        phase: "done",
                        request_kind: action_name.clone(),
                        elapsed_ms: Some(elapsed_ms),
                        error: None,
                    };
                    let response = InteractiveResponse::event_request_lifecycle(lifecycle);
                    let lifecycle_seq = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        response,
                        "request_lifecycle",
                        None,
                    )
                    .await?;
                    if subscription.wants("watchpoint_hit") && !watchpoints.is_empty() {
                        for hit in evaluate_watchpoints(
                            &mut watchpoints,
                            "request_lifecycle",
                            None,
                            lifecycle_seq,
                            None,
                            Some("done"),
                        ) {
                            let response = InteractiveResponse::event_watchpoint_hit(hit);
                            let _ = send_event_if_allowed(
                                &mut writer,
                                &mut sequencer,
                                &mut event_buffer,
                                &subscription,
                                &mut budget_state,
                                response,
                                "watchpoint_hit",
                                None,
                            )
                            .await?;
                        }
                    }
                }
                emit_screen_events(
                    &mut writer,
                    &mut sequencer,
                    &mut event_buffer,
                    &mut budget_state,
                    client,
                    inspector,
                    session_id,
                    attached,
                    &subscription,
                    &mut pane_cache,
                    &mut watchpoints,
                )
                .await?;
            }
            Err(err) => {
                let mut resp =
                    InteractiveResponse::fail(&action_name, elapsed_ms, format!("{err:#}"));

                // Extract structured failure data if available (same pattern as batch mode).
                if let Some(sf) = err.downcast_ref::<super::types::StepFailure>() {
                    resp.detail = Some(sf.message.clone());
                    if let Some(ref expected) = sf.expected {
                        resp.error = Some(format!("expected: {expected}"));
                    }
                    if let Some(ref actual) = sf.actual {
                        resp.detail = Some(format!("{}\nactual: {actual}", sf.message));
                    }
                }

                // Auto-capture pane states on failure.
                if *attached {
                    if let Some(sid) = *session_id {
                        // Refresh screen to get latest state.
                        let _ = inspector.refresh(client, sid).await;
                    }
                    resp.panes = inspector.capture_all_safe();
                }

                send_response(
                    &mut writer,
                    &mut sequencer,
                    resp,
                    "response",
                    active_request_id.as_deref(),
                )
                .await?;
                if subscription.active && subscription.wants("request_lifecycle") {
                    let lifecycle = RequestLifecycleEvent {
                        phase: "error",
                        request_kind: action_name.clone(),
                        elapsed_ms: Some(elapsed_ms),
                        error: Some(err.to_string()),
                    };
                    let response = InteractiveResponse::event_request_lifecycle(lifecycle);
                    let lifecycle_seq = send_event_if_allowed(
                        &mut writer,
                        &mut sequencer,
                        &mut event_buffer,
                        &subscription,
                        &mut budget_state,
                        response,
                        "request_lifecycle",
                        None,
                    )
                    .await?;
                    if subscription.wants("watchpoint_hit") && !watchpoints.is_empty() {
                        for hit in evaluate_watchpoints(
                            &mut watchpoints,
                            "request_lifecycle",
                            None,
                            lifecycle_seq,
                            None,
                            Some("error"),
                        ) {
                            let response = InteractiveResponse::event_watchpoint_hit(hit);
                            let _ = send_event_if_allowed(
                                &mut writer,
                                &mut sequencer,
                                &mut event_buffer,
                                &subscription,
                                &mut budget_state,
                                response,
                                "watchpoint_hit",
                                None,
                            )
                            .await?;
                        }
                    }
                }
                emit_screen_events(
                    &mut writer,
                    &mut sequencer,
                    &mut event_buffer,
                    &mut budget_state,
                    client,
                    inspector,
                    session_id,
                    attached,
                    &subscription,
                    &mut pane_cache,
                    &mut watchpoints,
                )
                .await?;
                // Don't break on failure — let the agent decide what to do.
            }
        }
    }

    // Clean up the output polling task if still running.
    if let Some(task) = output_task.take() {
        task.abort();
    }
    if let Some(task) = server_event_task.take() {
        task.abort();
    }

    Ok(())
}

/// Handle the `screen` special command — return all pane screen text.
async fn handle_screen_command(
    client: &mut bmux_client::BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &Option<Uuid>,
    attached: &bool,
) -> InteractiveResponse {
    let Some(sid) = *session_id else {
        return InteractiveResponse::error("no session — use new-session first".to_string());
    };
    if !*attached {
        return InteractiveResponse::error("not attached to a session".to_string());
    }

    match drain_and_capture(client, inspector, sid).await {
        Ok(panes) => {
            let mut resp = InteractiveResponse::ok("screen");
            resp.panes = Some(panes);
            resp
        }
        Err(e) => InteractiveResponse::error(format!("screen capture failed: {e:#}")),
    }
}

/// Handle the `status` special command — return session/pane metadata.
async fn handle_status_command(
    client: &mut bmux_client::BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &Option<Uuid>,
    attached: &bool,
) -> InteractiveResponse {
    let Some(sid) = *session_id else {
        return InteractiveResponse::error("no session — use new-session first".to_string());
    };
    if !*attached {
        return InteractiveResponse::error("not attached to a session".to_string());
    }

    match inspector.refresh(client, sid).await {
        Ok(snapshot) => {
            let pane_count = snapshot.panes.len() as u32;
            let focused = snapshot.panes.iter().find(|p| p.focused).map(|p| p.index);
            let mut resp = InteractiveResponse::ok("status");
            resp.session_id = Some(sid);
            resp.pane_count = Some(pane_count);
            resp.focused_pane = focused;
            resp
        }
        Err(e) => InteractiveResponse::error(format!("status query failed: {e:#}")),
    }
}

/// Drain output and capture all pane screen text.
async fn drain_and_capture(
    client: &mut bmux_client::BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: Uuid,
) -> Result<Vec<PaneCapture>> {
    drain_output_until_idle(client, session_id, Duration::from_millis(200), &mut None).await?;
    let _snapshot = inspector.refresh(client, session_id).await?;
    Ok(inspector.capture_all())
}

/// Write a JSON response line to the client.
async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: &InteractiveResponse,
) -> Result<()> {
    let json = serde_json::to_string(response)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn send_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    sequencer: &mut MessageSequencer,
    mut response: InteractiveResponse,
    message_type: &str,
    request_id: Option<&str>,
) -> Result<StoredMessage> {
    let seq = sequencer.stamp(&mut response, message_type, request_id);
    let payload = serde_json::to_value(&response)?;
    write_response(writer, &response).await?;
    Ok(StoredMessage {
        seq,
        event_type: response.event_type.clone(),
        payload,
    })
}

async fn send_response_buffered<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    sequencer: &mut MessageSequencer,
    event_buffer: &mut EventBuffer,
    response: InteractiveResponse,
    message_type: &str,
    request_id: Option<&str>,
) -> Result<u64> {
    let stored = send_response(writer, sequencer, response, message_type, request_id).await?;
    let seq = stored.seq;
    event_buffer.push(stored);
    Ok(seq)
}

async fn send_event_if_allowed<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    sequencer: &mut MessageSequencer,
    event_buffer: &mut EventBuffer,
    subscription: &LiveSubscription,
    budget_state: &mut EventBudgetState,
    response: InteractiveResponse,
    event_type: &str,
    pane_index: Option<u32>,
) -> Result<Option<u64>> {
    let approx_bytes = serde_json::to_vec(&response)?.len();
    if !budget_state.allows(subscription, event_type, pane_index, approx_bytes) {
        return Ok(None);
    }
    let seq =
        send_response_buffered(writer, sequencer, event_buffer, response, "event", None).await?;
    Ok(Some(seq))
}

fn update_pane_cache_from_inspector(
    inspector: &ScreenInspector,
    pane_cache: &mut std::collections::HashMap<u32, PaneCapture>,
) {
    pane_cache.clear();
    for pane in inspector.capture_all() {
        pane_cache.insert(pane.index, pane);
    }
}

async fn emit_screen_events<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    sequencer: &mut MessageSequencer,
    event_buffer: &mut EventBuffer,
    budget_state: &mut EventBudgetState,
    client: &mut bmux_client::BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &Option<Uuid>,
    attached: &bool,
    subscription: &LiveSubscription,
    pane_cache: &mut std::collections::HashMap<u32, PaneCapture>,
    watchpoints: &mut WatchpointRegistry,
) -> Result<()> {
    if !subscription.active
        || (!subscription.wants("cursor_delta")
            && !subscription.wants("screen_delta")
            && !subscription.wants("watchpoint_hit"))
    {
        return Ok(());
    }
    let Some(sid) = *session_id else {
        return Ok(());
    };
    if !*attached {
        return Ok(());
    }

    if inspector.refresh(client, sid).await.is_err() {
        return Ok(());
    }
    let deltas = inspector.build_deltas(pane_cache, subscription.resolved_delta_format());
    for delta in &deltas {
        if !subscription.allows_pane(delta.pane.index) {
            continue;
        }
        if let Some(cursor) = &delta.cursor_delta {
            let mut cursor_event_seq = None;
            if subscription.wants("cursor_delta") {
                let event = CursorDeltaEvent {
                    pane_index: cursor.pane_index,
                    from: CursorPosition {
                        row: cursor.from.row,
                        col: cursor.from.col,
                    },
                    to: CursorPosition {
                        row: cursor.to.row,
                        col: cursor.to.col,
                    },
                    distance: cursor.distance,
                };
                let response = InteractiveResponse::event_cursor_delta(event);
                cursor_event_seq = send_event_if_allowed(
                    writer,
                    sequencer,
                    event_buffer,
                    subscription,
                    budget_state,
                    response,
                    "cursor_delta",
                    Some(delta.pane.index),
                )
                .await?;
            }
            if subscription.wants("watchpoint_hit") && !watchpoints.is_empty() {
                for hit in evaluate_watchpoints(
                    watchpoints,
                    "cursor_delta",
                    Some(delta.pane.index),
                    cursor_event_seq,
                    Some(cursor.distance),
                    None,
                ) {
                    let response = InteractiveResponse::event_watchpoint_hit(hit);
                    let _ = send_event_if_allowed(
                        writer,
                        sequencer,
                        event_buffer,
                        subscription,
                        budget_state,
                        response,
                        "watchpoint_hit",
                        Some(delta.pane.index),
                    )
                    .await?;
                }
            }
        }
        if let Some(screen_delta) = &delta.screen_delta {
            let mut screen_delta_seq = None;
            if subscription.wants("screen_delta") {
                let response = InteractiveResponse::event_screen_delta(screen_delta.clone());
                screen_delta_seq = send_event_if_allowed(
                    writer,
                    sequencer,
                    event_buffer,
                    subscription,
                    budget_state,
                    response,
                    "screen_delta",
                    Some(delta.pane.index),
                )
                .await?;
            }
            if subscription.wants("watchpoint_hit") && !watchpoints.is_empty() {
                for hit in evaluate_watchpoints(
                    watchpoints,
                    "screen_delta",
                    Some(delta.pane.index),
                    screen_delta_seq,
                    None,
                    None,
                ) {
                    let response = InteractiveResponse::event_watchpoint_hit(hit);
                    let _ = send_event_if_allowed(
                        writer,
                        sequencer,
                        event_buffer,
                        subscription,
                        budget_state,
                        response,
                        "watchpoint_hit",
                        Some(delta.pane.index),
                    )
                    .await?;
                }
            }
        }
    }
    pane_cache.clear();
    for pane in deltas {
        pane_cache.insert(pane.pane.index, pane.pane);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_json_command<W: tokio::io::AsyncWrite + Unpin>(
    json: InteractiveJsonRequest,
    writer: &mut W,
    sequencer: &mut MessageSequencer,
    event_buffer: &mut EventBuffer,
    client: &mut bmux_client::BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    subscription: &mut LiveSubscription,
    output_task: &mut Option<tokio::task::JoinHandle<()>>,
    server_event_task: &mut Option<tokio::task::JoinHandle<()>>,
    output_tx: &tokio::sync::mpsc::Sender<(u32, String)>,
    server_event_tx: &tokio::sync::mpsc::Sender<bmux_client::ServerEvent>,
    pane_cache: &mut std::collections::HashMap<u32, PaneCapture>,
    sandbox: &super::sandbox::SandboxServer,
    watchpoints: &mut WatchpointRegistry,
) -> Result<bool> {
    match json.op.as_str() {
        "hello" => {
            if json.prefer_machine_readable.unwrap_or(false)
                || json
                    .client
                    .as_deref()
                    .is_some_and(|client| client.to_ascii_lowercase().contains("llm"))
            {
                subscription.prefer_machine_readable = true;
            }
            let mut resp = InteractiveResponse::ok("hello");
            resp.detail = Some("protocol_version=1".to_string());
            send_response(
                writer,
                sequencer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        "subscribe" => {
            let Some(sid) = *session_id else {
                let mut resp =
                    InteractiveResponse::error("no session — use new-session first".to_string());
                resp.code = Some("invalid_state".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            };
            if !*attached {
                let mut resp = InteractiveResponse::error("not attached to a session".to_string());
                resp.code = Some("invalid_state".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            }
            if output_task.is_none() {
                match sandbox.connect("bmux-playbook-output-stream").await {
                    Ok(mut event_client) => {
                        let tx = output_tx.clone();
                        let focused = inspector
                            .refresh(client, sid)
                            .await
                            .ok()
                            .and_then(|snapshot| {
                                snapshot
                                    .panes
                                    .iter()
                                    .find(|pane| pane.focused)
                                    .map(|pane| pane.index)
                            })
                            .unwrap_or(1);
                        *output_task = Some(tokio::spawn(async move {
                            output_poll_loop(&mut event_client, sid, focused, tx).await;
                        }));
                    }
                    Err(e) => {
                        let mut resp =
                            InteractiveResponse::error(format!("subscribe failed: {e:#}"));
                        resp.code = Some("internal".to_string());
                        send_response(writer, sequencer, resp, "error", json.request_id.as_deref())
                            .await?;
                        return Ok(true);
                    }
                }
            }
            if server_event_task.is_none() {
                match sandbox.connect("bmux-playbook-event-stream").await {
                    Ok(mut event_client) => {
                        if let Err(error) = event_client.subscribe_events().await {
                            let mut resp = InteractiveResponse::error(format!(
                                "server event subscribe failed: {error:#}"
                            ));
                            resp.code = Some("internal".to_string());
                            send_response_buffered(
                                writer,
                                sequencer,
                                event_buffer,
                                resp,
                                "error",
                                json.request_id.as_deref(),
                            )
                            .await?;
                            return Ok(true);
                        }
                        let tx = server_event_tx.clone();
                        *server_event_task = Some(tokio::spawn(async move {
                            server_event_poll_loop(&mut event_client, tx).await;
                        }));
                    }
                    Err(error) => {
                        let mut resp = InteractiveResponse::error(format!(
                            "server event stream connection failed: {error:#}"
                        ));
                        resp.code = Some("internal".to_string());
                        send_response_buffered(
                            writer,
                            sequencer,
                            event_buffer,
                            resp,
                            "error",
                            json.request_id.as_deref(),
                        )
                        .await?;
                        return Ok(true);
                    }
                }
            }

            subscription.active = true;
            subscription.event_types = if json.event_types.is_empty() {
                [
                    "pane_output".to_string(),
                    "pane_input".to_string(),
                    "cursor_delta".to_string(),
                    "screen_delta".to_string(),
                    "server_event".to_string(),
                    "request_lifecycle".to_string(),
                    "watchpoint_hit".to_string(),
                ]
                .into_iter()
                .collect()
            } else {
                json.event_types
                    .into_iter()
                    .map(|entry| entry.to_ascii_lowercase())
                    .collect()
            };
            subscription.pane_indexes = if json.pane_indexes.is_empty() {
                None
            } else {
                Some(json.pane_indexes.into_iter().collect())
            };
            subscription.max_events_per_sec = json.max_events_per_sec.or(Some(500));
            subscription.max_bytes_per_sec = json.max_bytes_per_sec.or(Some(256 * 1024));
            subscription.coalesce_ms = json.coalesce_ms.unwrap_or(0);
            subscription.format_preference =
                match parse_delta_preference(json.screen_delta_format.as_deref()) {
                    Ok(value) => value,
                    Err(error) => {
                        let mut resp = InteractiveResponse::error(error.to_string());
                        resp.code = Some("invalid_op".to_string());
                        send_response(writer, sequencer, resp, "error", json.request_id.as_deref())
                            .await?;
                        return Ok(true);
                    }
                };
            if json.prefer_machine_readable.unwrap_or(false)
                || json
                    .client
                    .as_deref()
                    .is_some_and(|client| client.to_ascii_lowercase().contains("llm"))
            {
                subscription.prefer_machine_readable = true;
            }

            let mut resp = InteractiveResponse::ok("subscribe");
            resp.subscription_id = Some(subscription.subscription_id.clone());
            resp.detail = Some(format!(
                "events={} screen_delta_format={:?}",
                subscription
                    .event_types
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                subscription.resolved_delta_format()
            ));
            send_response_buffered(
                writer,
                sequencer,
                event_buffer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        "unsubscribe" => {
            if let Some(task) = output_task.take() {
                task.abort();
            }
            if let Some(task) = server_event_task.take() {
                task.abort();
            }
            subscription.active = false;
            let resp = InteractiveResponse::ok("unsubscribe");
            send_response_buffered(
                writer,
                sequencer,
                event_buffer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        "set_watchpoint" => {
            let Some(id) = json.id.clone() else {
                let mut resp = InteractiveResponse::error("set_watchpoint requires id".to_string());
                resp.code = Some("invalid_op".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            };
            let kind = json.kind.as_deref().unwrap_or("event_burst");
            if kind != "event_burst" {
                let mut resp =
                    InteractiveResponse::error(format!("unsupported watchpoint kind: {kind}"));
                resp.code = Some("invalid_op".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            }

            let Some(event_type) = json.event_type.as_deref().map(str::to_ascii_lowercase) else {
                let mut resp =
                    InteractiveResponse::error("set_watchpoint requires event_type".to_string());
                resp.code = Some("invalid_op".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            };
            if !matches!(
                event_type.as_str(),
                "pane_output"
                    | "pane_input"
                    | "cursor_delta"
                    | "screen_delta"
                    | "server_event"
                    | "request_lifecycle"
            ) {
                let mut resp = InteractiveResponse::error(format!(
                    "unsupported event_type '{event_type}'; supported: pane_output,pane_input,cursor_delta,screen_delta,server_event,request_lifecycle"
                ));
                resp.code = Some("invalid_op".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            }

            let contains_regex = if let Some(raw_regex) = json.contains_regex.as_deref() {
                if event_type != "pane_output" {
                    let mut resp = InteractiveResponse::error(
                        "contains_regex is currently only supported for pane_output watchpoints"
                            .to_string(),
                    );
                    resp.code = Some("invalid_op".to_string());
                    send_response(writer, sequencer, resp, "error", json.request_id.as_deref())
                        .await?;
                    return Ok(true);
                }
                match Regex::new(raw_regex) {
                    Ok(compiled) => Some((raw_regex.to_string(), compiled)),
                    Err(error) => {
                        let mut resp =
                            InteractiveResponse::error(format!("invalid contains_regex: {error}"));
                        resp.code = Some("invalid_op".to_string());
                        send_response(writer, sequencer, resp, "error", json.request_id.as_deref())
                            .await?;
                        return Ok(true);
                    }
                }
            } else {
                None
            };

            let watchpoint = EventBurstWatchpoint {
                id: id.clone(),
                event_type: event_type.clone(),
                pane_index: json.pane_index,
                min_hits: json.min_hits.unwrap_or(3).max(1),
                window_ms: json.window_ms.unwrap_or(500).max(1),
                contains_regex_raw: contains_regex.as_ref().map(|(raw, _)| raw.clone()),
                contains_regex: contains_regex.map(|(_, compiled)| compiled),
            };
            watchpoints.upsert_event_burst(watchpoint.clone());
            let detail = format!(
                "id={} kind=event_burst event_type={} pane_index={} min_hits={} window_ms={} contains_regex={}",
                watchpoint.id,
                watchpoint.event_type,
                watchpoint
                    .pane_index
                    .map_or_else(|| "any".to_string(), |pane| pane.to_string()),
                watchpoint.min_hits,
                watchpoint.window_ms,
                watchpoint.contains_regex_raw.as_deref().unwrap_or("none")
            );

            let mut resp = InteractiveResponse::ok("set_watchpoint");
            resp.detail = Some(detail);
            send_response(
                writer,
                sequencer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        "clear_watchpoint" => {
            let Some(id) = json.id.as_deref() else {
                let mut resp =
                    InteractiveResponse::error("clear_watchpoint requires id".to_string());
                resp.code = Some("invalid_op".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            };
            let removed = watchpoints.clear(id);
            let mut resp = InteractiveResponse::ok("clear_watchpoint");
            resp.detail = Some(if removed {
                format!("removed watchpoint id={id}")
            } else {
                format!("watchpoint id={id} not found")
            });
            send_response(
                writer,
                sequencer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        "hydrate" => {
            if json.kind.as_deref() == Some("event_window") {
                let start = json.start_seq.unwrap_or(1);
                let end = json.end_seq.unwrap_or_else(|| event_buffer.latest_seq());
                let mut resp = InteractiveResponse::ok("hydrate");
                resp.detail = Some(format!("kind=event_window start_seq={start} end_seq={end}"));
                resp.event_window = Some(event_buffer.window(start, end));
                send_response_buffered(
                    writer,
                    sequencer,
                    event_buffer,
                    resp,
                    "response",
                    json.request_id.as_deref(),
                )
                .await?;
                return Ok(true);
            }
            if json.kind.as_deref() == Some("incident") {
                let center_seq = json
                    .around_seq
                    .or_else(|| {
                        json.id
                            .as_ref()
                            .and_then(|id| event_buffer.last_watchpoint_hit_seq.get(id).copied())
                    })
                    .unwrap_or_else(|| event_buffer.latest_seq());
                let radius = json.window_radius.unwrap_or(50);
                let mut resp = InteractiveResponse::ok("hydrate");
                resp.detail = Some(format!(
                    "kind=incident center_seq={center_seq} radius={radius}"
                ));
                resp.event_window = Some(event_buffer.around(center_seq, radius));
                send_response_buffered(
                    writer,
                    sequencer,
                    event_buffer,
                    resp,
                    "response",
                    json.request_id.as_deref(),
                )
                .await?;
                return Ok(true);
            }

            if json.kind.as_deref() != Some("screen_full") {
                let mut resp = InteractiveResponse::error("unsupported hydrate kind".to_string());
                resp.code = Some("invalid_op".to_string());
                send_response_buffered(
                    writer,
                    sequencer,
                    event_buffer,
                    resp,
                    "error",
                    json.request_id.as_deref(),
                )
                .await?;
                return Ok(true);
            }
            let resp = handle_screen_command(client, inspector, session_id, attached).await;
            update_pane_cache_from_inspector(inspector, pane_cache);
            let response = if let Some(target_pane) = json.pane_index {
                let mut single = resp;
                single.panes = single.panes.map(|panes| {
                    panes
                        .into_iter()
                        .filter(|pane| pane.index == target_pane)
                        .collect()
                });
                single
            } else {
                resp
            };
            send_response_buffered(
                writer,
                sequencer,
                event_buffer,
                response,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        "status" => {
            let resp = handle_status_command(client, inspector, session_id, attached).await;
            update_pane_cache_from_inspector(inspector, pane_cache);
            send_response(
                writer,
                sequencer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        "quit" => {
            let resp = InteractiveResponse::ok("quit");
            send_response(
                writer,
                sequencer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(false)
        }
        "command" => {
            let Some(dsl) = json.dsl.as_deref() else {
                let mut resp = InteractiveResponse::error("command requires dsl".to_string());
                resp.code = Some("invalid_op".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            };
            let line = dsl.trim().to_string();
            if line.is_empty() {
                let mut resp =
                    InteractiveResponse::error("command dsl cannot be empty".to_string());
                resp.code = Some("invalid_op".to_string());
                send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
                return Ok(true);
            }
            let mut resp = InteractiveResponse::ok("command");
            resp.detail = Some(format!("dsl={line}"));
            send_response(
                writer,
                sequencer,
                resp,
                "response",
                json.request_id.as_deref(),
            )
            .await?;
            Ok(true)
        }
        _ => {
            let mut resp = InteractiveResponse::error(format!("unsupported op: {}", json.op));
            resp.code = Some("invalid_op".to_string());
            send_response(writer, sequencer, resp, "error", json.request_id.as_deref()).await?;
            Ok(true)
        }
    }
}

fn parse_delta_preference(value: Option<&str>) -> Result<DeltaFormatPreference> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None | Some("auto") => Ok(DeltaFormatPreference::Auto),
        Some("line_ops") => Ok(DeltaFormatPreference::LineOps),
        Some("unified_diff") => Ok(DeltaFormatPreference::UnifiedDiff),
        Some(other) => anyhow::bail!("invalid screen_delta_format '{other}'"),
    }
}

fn evaluate_watchpoints(
    watchpoints: &mut WatchpointRegistry,
    event_type: &str,
    pane_index: Option<u32>,
    event_seq: Option<u64>,
    peak_distance: Option<u16>,
    event_text: Option<&str>,
) -> Vec<WatchpointHitEvent> {
    let mut hits = Vec::new();
    let now = Instant::now();
    if event_type == "watchpoint_hit" {
        return hits;
    }

    for watchpoint in &watchpoints.event_burst {
        if watchpoint.event_type != event_type {
            continue;
        }
        if watchpoint.pane_index.is_some() && pane_index != watchpoint.pane_index {
            continue;
        }
        if let Some(regex) = &watchpoint.contains_regex {
            let Some(text) = event_text else {
                continue;
            };
            if !regex.is_match(text) {
                continue;
            }
        }

        let pane_scope_key = pane_index.unwrap_or(0);
        let key = (watchpoint.id.clone(), pane_scope_key);
        let history = watchpoints.burst_history.entry(key).or_default();
        history.push_back(EventBurstSample {
            at: now,
            peak_distance,
            seq: event_seq,
        });
        while let Some(front) = history.front() {
            if now.duration_since(front.at).as_millis() as u64 <= watchpoint.window_ms {
                break;
            }
            history.pop_front();
        }
        if history.len() >= usize::from(watchpoint.min_hits) {
            let observed_hits = history.len() as u16;
            let peak_distance = history
                .iter()
                .filter_map(|entry| entry.peak_distance)
                .max()
                .or(peak_distance);
            let evidence_seq_start = history.front().and_then(|entry| entry.seq);
            let evidence_seq_end = history.back().and_then(|entry| entry.seq);

            hits.push(WatchpointHitEvent {
                id: watchpoint.id.clone(),
                kind: "event_burst",
                watch_event_type: watchpoint.event_type.clone(),
                pane_index,
                summary: format!(
                    "event burst detected: event_type={} hits={} min_hits={} pane={}",
                    watchpoint.event_type,
                    observed_hits,
                    watchpoint.min_hits,
                    pane_index.map_or_else(|| "any".to_string(), |pane| pane.to_string())
                ),
                min_hits: watchpoint.min_hits,
                observed_hits,
                window_ms: watchpoint.window_ms,
                peak_distance,
                evidence_seq_start,
                evidence_seq_end,
            });

            history.clear();
        }
    }

    hits
}

fn pane_input_from_action(action: &super::types::Action) -> Option<PaneInputEvent> {
    match action {
        super::types::Action::SendKeys { keys, pane } => Some(PaneInputEvent {
            pane_index: *pane,
            byte_len: keys.len(),
            printable_preview: String::from_utf8_lossy(keys).to_string(),
        }),
        super::types::Action::SendBytes { hex } => Some(PaneInputEvent {
            pane_index: None,
            byte_len: hex.len(),
            printable_preview: String::from_utf8_lossy(hex).to_string(),
        }),
        super::types::Action::SendAttach { key } => Some(PaneInputEvent {
            pane_index: None,
            byte_len: 0,
            printable_preview: format!("<attach:{key}>"),
        }),
        super::types::Action::PrefixKey { key } => Some(PaneInputEvent {
            pane_index: None,
            byte_len: 2,
            printable_preview: format!("<prefix>{key}"),
        }),
        _ => None,
    }
}

fn server_event_name(event: &bmux_client::ServerEvent) -> &'static str {
    match event {
        bmux_client::ServerEvent::ServerStarted => "server_started",
        bmux_client::ServerEvent::ServerStopping => "server_stopping",
        bmux_client::ServerEvent::SessionCreated { .. } => "session_created",
        bmux_client::ServerEvent::SessionRemoved { .. } => "session_removed",
        bmux_client::ServerEvent::ClientAttached { .. } => "client_attached",
        bmux_client::ServerEvent::ClientDetached { .. } => "client_detached",
        bmux_client::ServerEvent::FollowStarted { .. } => "follow_started",
        bmux_client::ServerEvent::FollowStopped { .. } => "follow_stopped",
        bmux_client::ServerEvent::FollowTargetGone { .. } => "follow_target_gone",
        bmux_client::ServerEvent::FollowTargetChanged { .. } => "follow_target_changed",
        bmux_client::ServerEvent::AttachViewChanged { .. } => "attach_view_changed",
        bmux_client::ServerEvent::PaneOutputAvailable { .. } => "pane_output_available",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_burst_regex_predicate_filters_pane_output() {
        let mut watchpoints = WatchpointRegistry::default();
        watchpoints.upsert_event_burst(EventBurstWatchpoint {
            id: "pane-output-regex".to_string(),
            event_type: "pane_output".to_string(),
            pane_index: Some(1),
            min_hits: 1,
            window_ms: 500,
            contains_regex_raw: Some("match_me_[0-9]+".to_string()),
            contains_regex: Some(Regex::new("match_me_[0-9]+$").expect("valid regex")),
        });

        let no_match_hits = evaluate_watchpoints(
            &mut watchpoints,
            "pane_output",
            Some(1),
            Some(1),
            None,
            Some("no_match_here"),
        );
        assert!(no_match_hits.is_empty());

        let match_hits = evaluate_watchpoints(
            &mut watchpoints,
            "pane_output",
            Some(1),
            Some(2),
            None,
            Some("match_me_123"),
        );
        assert_eq!(match_hits.len(), 1);
        assert_eq!(match_hits[0].kind, "event_burst");
        assert_eq!(match_hits[0].watch_event_type, "pane_output");
    }
}

// ── Endpoint selection ────────────────────────────────────────────────────────

/// Create a cross-platform IPC endpoint for the interactive session.
///
/// On Unix, this is a Unix socket in the sandbox temp directory.
/// On Windows, this is a named pipe derived from the sandbox root path.
fn interactive_endpoint(
    socket_override: Option<&str>,
    sandbox: &SandboxServer,
) -> bmux_ipc::IpcEndpoint {
    if let Some(user_path) = socket_override {
        // User-specified path — treat as Unix socket on Unix, named pipe on Windows.
        #[cfg(unix)]
        {
            return bmux_ipc::IpcEndpoint::unix_socket(user_path);
        }
        #[cfg(windows)]
        {
            return bmux_ipc::IpcEndpoint::windows_named_pipe(user_path.to_string());
        }
        #[cfg(not(any(unix, windows)))]
        {
            return bmux_ipc::IpcEndpoint::unix_socket(user_path);
        }
    }

    // Auto-generated endpoint from sandbox root.
    #[cfg(unix)]
    {
        bmux_ipc::IpcEndpoint::unix_socket(sandbox.root_dir().join("playbook.sock"))
    }
    #[cfg(windows)]
    {
        // Generate a unique named pipe from the sandbox root path.
        let root_str = sandbox.root_dir().to_string_lossy();
        let hash = simple_hash(root_str.as_bytes());
        bmux_ipc::IpcEndpoint::windows_named_pipe(format!(r"\\.\pipe\bmux-playbook-{hash:016x}"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        bmux_ipc::IpcEndpoint::unix_socket(sandbox.root_dir().join("playbook.sock"))
    }
}

/// Simple FNV-1a hash for generating stable, unique pipe names.
#[cfg(windows)]
fn simple_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Background task that polls for output from the sandbox and sends push
/// events through the channel. Runs until the channel is closed (receiver dropped)
/// or the task is aborted.
async fn output_poll_loop(
    client: &mut bmux_client::BmuxClient,
    session_id: Uuid,
    focused_pane_index: u32,
    tx: tokio::sync::mpsc::Sender<(u32, String)>,
) {
    const MAX_OUTPUT_BYTES: usize = 64 * 1024;
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    loop {
        match client.attach_output(session_id, MAX_OUTPUT_BYTES).await {
            Ok(data) if !data.is_empty() => {
                let text = String::from_utf8_lossy(&data).to_string();
                // attach_output returns output from the focused pane.
                if tx.send((focused_pane_index, text)).await.is_err() {
                    break; // Receiver dropped — unsubscribed or session ended
                }
            }
            Ok(_) => {
                // No output — sleep before polling again.
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(e) => {
                warn!("output poll error: {e}");
                break;
            }
        }
    }
}

async fn server_event_poll_loop(
    client: &mut bmux_client::BmuxClient,
    tx: tokio::sync::mpsc::Sender<bmux_client::ServerEvent>,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    loop {
        match client.poll_events(64).await {
            Ok(events) if !events.is_empty() => {
                for event in events {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
            Ok(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(error) => {
                warn!("server event poll error: {error}");
                return;
            }
        }
    }
}
