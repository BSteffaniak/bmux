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
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{info, warn};
use uuid::Uuid;

use super::engine::{drain_output_until_idle, execute_step, start_recording};
use super::parse_dsl::parse_action_line;
use super::sandbox::SandboxServer;
use super::screen::ScreenInspector;
use super::types::{PaneCapture, SnapshotCapture, Step};

/// Default timeout for sandbox server startup.
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// JSON response sent back to the agent for each command.
#[derive(Serialize)]
struct InteractiveResponse {
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
}

impl InteractiveResponse {
    fn ok(action: &str) -> Self {
        Self {
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
        }
    }

    fn ok_with_detail(action: &str, elapsed_ms: u64, detail: Option<String>) -> Self {
        Self {
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
        }
    }

    fn fail(action: &str, elapsed_ms: u64, error: String) -> Self {
        Self {
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
        }
    }

    fn error(message: String) -> Self {
        Self {
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
        }
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
    let sandbox = SandboxServer::start(
        shell,
        &plugins,
        SERVER_STARTUP_TIMEOUT,
        &std::collections::BTreeMap::new(),
        super::types::SandboxEnvMode::Inherit,
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
        &mut recording_id,
        &mut step_counter,
        &mut snapshots,
        viewport_cols,
        viewport_rows,
        record,
        deadline,
        &mut runtime_vars,
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
    recording_id: &mut Option<Uuid>,
    step_counter: &mut usize,
    snapshots: &mut Vec<SnapshotCapture>,
    viewport_cols: u16,
    viewport_rows: u16,
    record: bool,
    deadline: Option<Instant>,
    runtime_vars: &mut super::subst::RuntimeVars,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        // Check session timeout.
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                let resp = InteractiveResponse::error("session timeout exceeded".to_string());
                write_response(&mut writer, &resp).await?;
                break;
            }
        }

        // Read next command line.
        line.clear();
        let read_result = if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, reader.read_line(&mut line)).await {
                Ok(result) => result,
                Err(_) => {
                    let resp = InteractiveResponse::error("session timeout exceeded".to_string());
                    write_response(&mut writer, &resp).await?;
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

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Handle special commands.
        match trimmed {
            "quit" => {
                let resp = InteractiveResponse::ok("quit");
                write_response(&mut writer, &resp).await?;
                break;
            }
            "screen" => {
                let resp = handle_screen_command(client, inspector, session_id, attached).await;
                write_response(&mut writer, &resp).await?;
                continue;
            }
            "status" => {
                let resp = handle_status_command(client, inspector, session_id, attached).await;
                write_response(&mut writer, &resp).await?;
                continue;
            }
            "help" => {
                let resp = InteractiveResponse {
                    status: "ok",
                    action: Some("help".to_string()),
                    detail: Some(
                        "commands: quit, screen, status, help, or any DSL action \
                         (new-session, send-keys, wait-for, assert-screen, snapshot, \
                         assert-layout, assert-cursor, screen, status, etc.)"
                            .to_string(),
                    ),
                    ..InteractiveResponse::ok("help")
                };
                write_response(&mut writer, &resp).await?;
                continue;
            }
            _ => {}
        }

        // Parse as DSL action line.
        let action = match parse_action_line(trimmed) {
            Ok(action) => action,
            Err(e) => {
                let resp = InteractiveResponse::error(format!("{e:#}"));
                write_response(&mut writer, &resp).await?;
                continue;
            }
        };

        let action_name = action.name().to_string();
        let is_new_session = matches!(action, super::types::Action::NewSession { .. });

        let step = Step {
            index: *step_counter,
            action,
        };
        *step_counter += 1;

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

                write_response(&mut writer, &resp).await?;
            }
            Err(err) => {
                let resp = InteractiveResponse::fail(&action_name, elapsed_ms, format!("{err:#}"));
                write_response(&mut writer, &resp).await?;
                // Don't break on failure — let the agent decide what to do.
            }
        }
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
