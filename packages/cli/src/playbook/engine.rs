//! Playbook execution engine.
//!
//! Orchestrates the full lifecycle: parse → sandbox → execute steps → report.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bmux_client::BmuxClient;
use bmux_ipc::{InvokeServiceKind, PaneSplitDirection, SessionSelector};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::sandbox::SandboxServer;
use super::screen::ScreenInspector;
use super::subst::RuntimeVars;
use super::types::{
    Action, Playbook, PlaybookResult, ServiceKind, SnapshotCapture, SplitDirection, Step,
    StepResult, StepStatus,
};

/// Default timeout for waiting for the sandbox server to start.
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Max bytes to read from attach output per drain cycle.
const ATTACH_OUTPUT_MAX_BYTES: usize = 256 * 1024;

/// Run a playbook to completion, returning the result.
///
/// Handles Ctrl+C gracefully: on signal, the sandbox server is cleaned up
/// via `SandboxServer`'s `Drop` impl.
pub async fn run_playbook(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    tokio::select! {
        result = run_playbook_inner(playbook, target_server) => result,
        _ = tokio::signal::ctrl_c() => {
            // The sandbox (if any) will be cleaned up by Drop when the
            // run_playbook_inner future is dropped by select!.
            info!("playbook interrupted by signal");
            Err(anyhow::anyhow!("interrupted by signal"))
        }
    }
}

/// Core playbook execution logic.
async fn run_playbook_inner(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    let started = Instant::now();
    let playbook_name = playbook.config.name.clone();
    let should_record = playbook.config.record;

    let mut step_results = Vec::new();
    let mut snapshots = Vec::new();
    let mut error_msg: Option<String> = None;
    let mut recording_id: Option<Uuid> = None;

    // Either connect to an existing server or spin up a sandbox.
    let sandbox: Option<SandboxServer>;
    let mut client: BmuxClient;

    if target_server {
        sandbox = None;
        client = BmuxClient::connect_default("bmux-playbook-runner")
            .await
            .map_err(|e| anyhow::anyhow!("failed connecting to live server: {e}"))?;
    } else {
        let sb = SandboxServer::start(
            playbook.config.shell.as_deref(),
            &playbook.config.plugins,
            SERVER_STARTUP_TIMEOUT,
            &playbook.config.env,
            playbook.config.effective_env_mode(),
        )
        .await
        .context("failed starting sandbox server")?;
        client = sb.connect("bmux-playbook-runner").await?;
        sandbox = Some(sb);
    }

    let mut inspector =
        ScreenInspector::new(playbook.config.viewport.cols, playbook.config.viewport.rows);

    // Runtime variable context for substitution
    let mut runtime_vars = RuntimeVars::new(playbook.config.vars.clone());

    // Session tracking
    let mut session_id: Option<Uuid> = None;
    let mut attached = false;
    let mut events_subscribed = false;
    let mut display_track: Option<super::display_track::PlaybookDisplayTrackWriter> = None;

    // Start recording before any steps execute so that all events (including
    // NewSession) are captured. Uses session_id: None since no session exists
    // yet — the sandbox is ephemeral so there's no noise from other sessions.
    if should_record {
        match start_recording(&mut client, None).await {
            Ok(rid) => {
                info!("recording started: {rid}");
                recording_id = Some(rid);

                // Create display track writer for GIF export.
                if let Some(ref sb) = sandbox {
                    let rec_dir = sb.paths().recordings_dir().join(rid.to_string());
                    let client_id = match client.whoami().await {
                        Ok(id) => id,
                        Err(_) => Uuid::new_v4(),
                    };
                    match super::display_track::PlaybookDisplayTrackWriter::new(
                        &rec_dir,
                        client_id,
                        rid,
                        playbook.config.viewport.cols,
                        playbook.config.viewport.rows,
                    ) {
                        Ok(dt) => {
                            display_track = Some(dt);
                        }
                        Err(e) => {
                            warn!("failed to create display track: {e:#}");
                        }
                    }
                }
            }
            Err(e) => {
                warn!("failed to start recording: {e:#}");
                // Non-fatal — continue without recording.
            }
        }
    }

    // Execute each step
    let deadline = Instant::now() + playbook.config.timeout;

    for step in &playbook.steps {
        if Instant::now() > deadline {
            error_msg = Some("playbook timeout exceeded".to_string());
            step_results.push(StepResult {
                index: step.index,
                action: step.action.name().to_string(),
                status: StepStatus::Skip,
                elapsed_ms: 0,
                detail: Some("skipped: playbook timeout".to_string()),
            });
            continue;
        }

        let step_start = Instant::now();
        let result = execute_step(
            step,
            &mut client,
            &mut inspector,
            &mut session_id,
            &mut attached,
            &mut events_subscribed,
            &playbook.config.viewport.cols,
            &playbook.config.viewport.rows,
            &mut snapshots,
            deadline,
            &mut display_track,
            &mut runtime_vars,
        )
        .await;

        let elapsed_ms = step_start.elapsed().as_millis() as u64;

        match result {
            Ok(detail) => {
                info!(
                    "step {}: {} — pass ({}ms)",
                    step.index,
                    step.action.name(),
                    elapsed_ms
                );
                step_results.push(StepResult {
                    index: step.index,
                    action: step.action.name().to_string(),
                    status: StepStatus::Pass,
                    elapsed_ms,
                    detail,
                });
            }
            Err(err) => {
                warn!(
                    "step {}: {} — fail: {err:#} ({}ms)",
                    step.index,
                    step.action.name(),
                    elapsed_ms
                );
                let msg = format!("{err:#}");
                step_results.push(StepResult {
                    index: step.index,
                    action: step.action.name().to_string(),
                    status: StepStatus::Fail,
                    elapsed_ms,
                    detail: Some(msg.clone()),
                });
                error_msg = Some(msg);
                break; // Stop on first failure
            }
        }
    }

    // Finish display track before stopping the recording.
    if let Some(ref mut dt) = display_track {
        if let Err(e) = dt.finish() {
            warn!("failed to finish display track: {e:#}");
        }
    }

    // Copy recording dir to user recordings dir before sandbox shutdown.
    let mut recording_path: Option<std::path::PathBuf> = None;
    if let (Some(rid), Some(sb)) = (recording_id, &sandbox) {
        // Stop recording first so the server finalizes the binary files.
        match client.recording_stop(Some(rid)).await {
            Ok(stopped_id) => {
                info!("recording stopped: {stopped_id}");
            }
            Err(e) => {
                warn!("failed to stop recording: {e}");
            }
        }

        // Copy recording dir from sandbox to user recordings dir.
        let src_dir = sb.paths().recordings_dir().join(rid.to_string());
        let user_recordings = bmux_config::ConfigPaths::default().recordings_dir();
        let dest_dir = user_recordings.join(rid.to_string());

        if src_dir.exists() {
            if let Err(e) = copy_dir_recursive(&src_dir, &dest_dir) {
                warn!("failed to copy recording to user dir: {e:#}");
            } else {
                info!("recording copied to {}", dest_dir.display());
                recording_path = Some(dest_dir);
            }
        }
    }

    let total_elapsed_ms = started.elapsed().as_millis() as u64;
    let pass = error_msg.is_none();

    // Shutdown sandbox if we created one.
    if let Some(sb) = sandbox {
        if let Err(e) = sb.shutdown(!pass).await {
            warn!("sandbox shutdown error: {e:#}");
        }
    }

    Ok(PlaybookResult {
        playbook_name,
        pass,
        steps: step_results,
        snapshots,
        recording_id,
        recording_path: recording_path.map(|p| p.to_string_lossy().to_string()),
        total_elapsed_ms,
        error: error_msg,
    })
}

/// Start a recording on the server, optionally filtered to a specific session.
pub(super) async fn start_recording(
    client: &mut BmuxClient,
    session_id: Option<Uuid>,
) -> Result<Uuid> {
    let summary = client
        .recording_start(
            session_id, true, // capture_input
            None, // profile: server default (Functional)
            None, // event_kinds: server default
        )
        .await
        .map_err(|e| anyhow::anyhow!("recording start failed: {e}"))?;
    Ok(summary.id)
}

/// Execute a single step.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_step(
    step: &Step,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    events_subscribed: &mut bool,
    viewport_cols: &u16,
    viewport_rows: &u16,
    snapshots: &mut Vec<SnapshotCapture>,
    deadline: Instant,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    runtime_vars: &mut RuntimeVars,
) -> Result<Option<String>> {
    match &step.action {
        Action::NewSession { name } => {
            let resolved_name = name.as_ref().map(|n| runtime_vars.resolve_opt(n));
            let sid = client
                .new_session(resolved_name.clone())
                .await
                .map_err(|e| anyhow::anyhow!("new-session failed: {e}"))?;
            debug!("created session {sid}");

            // Update runtime vars
            runtime_vars.session_id = Some(sid);
            runtime_vars.session_name = resolved_name;
            runtime_vars.pane_count = 1;
            runtime_vars.focused_pane = 1;

            // Attach to the session
            let grant = client
                .attach_grant(SessionSelector::ById(sid))
                .await
                .map_err(|e| anyhow::anyhow!("attach grant failed: {e}"))?;
            client
                .open_attach_stream(&grant)
                .await
                .map_err(|e| anyhow::anyhow!("attach open failed: {e}"))?;
            client
                .attach_set_viewport(sid, *viewport_cols, *viewport_rows)
                .await
                .map_err(|e| anyhow::anyhow!("set viewport failed: {e}"))?;

            *session_id = Some(sid);
            *attached = true;

            // Drain initial output to let the shell start up
            drain_output_until_idle(client, sid, Duration::from_millis(500), display_track).await?;

            Ok(Some(format!("session_id={sid}")))
        }

        Action::KillSession { name } => {
            let selector = SessionSelector::ByName(name.clone());
            client
                .kill_session(selector)
                .await
                .map_err(|e| anyhow::anyhow!("kill-session failed: {e}"))?;
            if session_id.map(|_| true).unwrap_or(false) {
                // If we killed the session we were attached to, clear state
                *session_id = None;
                *attached = false;
            }
            Ok(None)
        }

        Action::SplitPane {
            direction,
            ratio: _,
        } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            let ipc_dir = match direction {
                SplitDirection::Vertical => PaneSplitDirection::Vertical,
                SplitDirection::Horizontal => PaneSplitDirection::Horizontal,
            };
            let pane_id = client
                .split_pane(Some(SessionSelector::ById(sid)), ipc_dir)
                .await
                .map_err(|e| anyhow::anyhow!("split-pane failed: {e}"))?;

            // Let the new pane shell start
            drain_output_until_idle(client, sid, Duration::from_millis(300), display_track).await?;

            runtime_vars.pane_count += 1;

            Ok(Some(format!("pane_id={pane_id}")))
        }

        Action::FocusPane { target } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            let selector = bmux_ipc::PaneSelector::ByIndex(*target);
            client
                .focus_pane_target(Some(SessionSelector::ById(sid)), selector)
                .await
                .map_err(|e| anyhow::anyhow!("focus-pane failed: {e}"))?;
            runtime_vars.focused_pane = *target;
            Ok(None)
        }

        Action::ClosePane { target } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            match target {
                Some(idx) => {
                    let selector = bmux_ipc::PaneSelector::ByIndex(*idx);
                    client
                        .close_pane_target(Some(SessionSelector::ById(sid)), selector)
                        .await
                        .map_err(|e| anyhow::anyhow!("close-pane failed: {e}"))?;
                }
                None => {
                    client
                        .close_pane(Some(SessionSelector::ById(sid)))
                        .await
                        .map_err(|e| anyhow::anyhow!("close-pane failed: {e}"))?;
                }
            }
            runtime_vars.pane_count = runtime_vars.pane_count.saturating_sub(1);
            Ok(None)
        }

        Action::SendKeys { keys, pane } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            let resolved_keys = runtime_vars.resolve_bytes(keys);

            if let Some(target_index) = pane {
                // Pane-targeted send: resolve the pane index to a UUID and use
                // PaneDirectInput to write bytes directly without focus changes.
                let snapshot = client
                    .attach_snapshot(sid, 0)
                    .await
                    .map_err(|e| anyhow::anyhow!("snapshot for pane lookup failed: {e}"))?;
                let pane_id = snapshot
                    .panes
                    .iter()
                    .find(|p| p.index == *target_index)
                    .map(|p| p.id)
                    .ok_or_else(|| anyhow::anyhow!("pane index {target_index} not found"))?;

                client
                    .pane_direct_input(sid, pane_id, resolved_keys.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("send-keys to pane {target_index} failed: {e}"))?;
            } else {
                client
                    .attach_input(sid, resolved_keys)
                    .await
                    .map_err(|e| anyhow::anyhow!("send-keys failed: {e}"))?;
            }
            Ok(None)
        }

        Action::SendBytes { hex } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            client
                .attach_input(sid, hex.clone())
                .await
                .map_err(|e| anyhow::anyhow!("send-bytes failed: {e}"))?;
            Ok(None)
        }

        Action::Sleep { duration } => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let sleep_dur = (*duration).min(remaining);
            tokio::time::sleep(sleep_dur).await;
            Ok(None)
        }

        Action::WaitFor {
            pattern,
            pane,
            timeout,
        } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            // Resolve variables in the pattern before compiling the regex.
            let resolved_pattern = runtime_vars.resolve_opt(pattern);

            // Compile regex once, not on every poll iteration.
            let re = regex::Regex::new(&resolved_pattern)
                .with_context(|| format!("invalid regex: {resolved_pattern}"))?;

            let wait_deadline = Instant::now() + (*timeout).min(deadline - Instant::now());
            let mut poll_delay = Duration::from_millis(10);

            loop {
                // Drain any pending output (lower threshold for WaitFor's retry loop)
                drain_output_with_threshold(
                    client,
                    sid,
                    Duration::from_millis(100),
                    display_track,
                    3,
                )
                .await?;

                // Refresh screen state
                let snapshot = inspector.refresh(client, sid).await?;
                let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

                if inspector.pane_matches_compiled(pane_idx, &re) {
                    return Ok(Some(format!("matched pattern '{resolved_pattern}'")));
                }

                if Instant::now() >= wait_deadline {
                    let screen_text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    // Truncate for readability
                    let truncated = if screen_text.len() > 200 {
                        format!("{}...", &screen_text[..200])
                    } else {
                        screen_text
                    };
                    bail!(
                        "wait-for timed out after {}ms on pane {} waiting for pattern '{}'; screen: {truncated}",
                        timeout.as_millis(),
                        pane_idx,
                        pattern
                    );
                }

                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(200));
            }
        }

        Action::Snapshot { id } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            drain_output_until_idle(client, sid, Duration::from_millis(200), display_track).await?;
            let _snapshot = inspector.refresh(client, sid).await?;
            let panes = inspector.capture_all();

            snapshots.push(SnapshotCapture {
                id: id.clone(),
                panes,
            });

            Ok(Some(format!("snapshot '{id}' captured")))
        }

        Action::AssertScreen {
            pane,
            contains,
            not_contains,
            matches,
        } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            drain_output_until_idle(client, sid, Duration::from_millis(200), display_track).await?;
            let snapshot = inspector.refresh(client, sid).await?;
            let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

            if let Some(needle) = contains {
                let resolved = runtime_vars.resolve_opt(needle);
                if !inspector.pane_contains(pane_idx, &resolved) {
                    let text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    bail!(
                        "assert-screen: pane {pane_idx} does not contain '{resolved}'; screen: {text}"
                    );
                }
            }

            if let Some(needle) = not_contains {
                let resolved = runtime_vars.resolve_opt(needle);
                if inspector.pane_contains(pane_idx, &resolved) {
                    bail!("assert-screen: pane {pane_idx} unexpectedly contains '{resolved}'");
                }
            }

            if let Some(pattern) = matches {
                let resolved = runtime_vars.resolve_opt(pattern);
                if !inspector.pane_matches(pane_idx, &resolved)? {
                    let text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    bail!(
                        "assert-screen: pane {pane_idx} does not match '{resolved}'; screen: {text}"
                    );
                }
            }

            Ok(None)
        }

        Action::AssertLayout { pane_count } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            let snapshot = inspector.refresh(client, sid).await?;
            let actual_count = snapshot.panes.len() as u32;

            if let Some(expected) = pane_count {
                if actual_count != *expected {
                    bail!("assert-layout: expected {expected} panes, got {actual_count}");
                }
            }

            Ok(None)
        }

        Action::AssertCursor { pane, row, col } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            let snapshot = inspector.refresh(client, sid).await?;
            let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

            let (actual_row, actual_col) = inspector
                .pane_cursor(pane_idx)
                .context("pane cursor not available")?;

            if actual_row != *row || actual_col != *col {
                bail!("assert-cursor: expected ({row},{col}), got ({actual_row},{actual_col})");
            }

            Ok(None)
        }

        Action::ResizeViewport { cols, rows } => {
            let sid = require_session(*session_id)?;
            if *attached {
                client
                    .attach_set_viewport(sid, *cols, *rows)
                    .await
                    .map_err(|e| anyhow::anyhow!("resize-viewport failed: {e}"))?;
            }
            inspector.update_viewport(*cols, *rows);
            if let Some(ref mut dt) = *display_track {
                let _ = dt.record_resize(*cols, *rows);
            }
            Ok(None)
        }

        Action::PrefixKey { key } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            // Default prefix is Ctrl+A (0x01), then the key character.
            let mut bytes = vec![0x01u8];
            let mut buf = [0u8; 4];
            let encoded = key.encode_utf8(&mut buf);
            bytes.extend_from_slice(encoded.as_bytes());
            client
                .attach_input(sid, bytes)
                .await
                .map_err(|e| anyhow::anyhow!("prefix-key failed: {e}"))?;
            Ok(None)
        }

        Action::WaitForEvent { event, timeout } => {
            let _sid = require_session(*session_id)?;

            // Subscribe to events on first use.
            if !*events_subscribed {
                client
                    .subscribe_events()
                    .await
                    .map_err(|e| anyhow::anyhow!("event subscription failed: {e}"))?;
                *events_subscribed = true;
            }

            let resolved_event = runtime_vars.resolve_opt(event);
            let wait_deadline = Instant::now() + (*timeout).min(deadline - Instant::now());
            let mut poll_delay = Duration::from_millis(25);

            loop {
                let events = client
                    .poll_events(32)
                    .await
                    .map_err(|e| anyhow::anyhow!("poll events failed: {e}"))?;

                for evt in &events {
                    if event_matches(evt, &resolved_event) {
                        return Ok(Some(format!("matched event '{resolved_event}'")));
                    }
                }

                if Instant::now() >= wait_deadline {
                    bail!(
                        "wait-for-event timed out after {}ms waiting for '{resolved_event}'",
                        timeout.as_millis()
                    );
                }

                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
            }
        }

        Action::InvokeService {
            capability,
            kind,
            interface_id,
            operation,
            payload,
        } => {
            let resolved_payload = runtime_vars.resolve_opt(payload);
            let ipc_kind = match kind {
                ServiceKind::Query => InvokeServiceKind::Query,
                ServiceKind::Command => InvokeServiceKind::Command,
            };
            let response_bytes = client
                .invoke_service_raw(
                    capability.clone(),
                    ipc_kind,
                    interface_id.clone(),
                    operation.clone(),
                    resolved_payload.into_bytes(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("invoke-service failed: {e}"))?;

            let detail = if response_bytes.is_empty() {
                None
            } else {
                Some(
                    String::from_utf8(response_bytes)
                        .unwrap_or_else(|e| format!("<{} bytes binary>", e.into_bytes().len())),
                )
            };
            Ok(detail)
        }

        Action::Screen => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            drain_output_until_idle(client, sid, Duration::from_millis(200), display_track).await?;
            let snapshot = inspector.refresh(client, sid).await?;
            let _ = snapshot; // satisfy the borrow checker
            let captures = inspector.capture_all();
            // Serialize the pane captures as JSON for inclusion in step detail.
            let json = serde_json::to_string(&captures).unwrap_or_else(|_| "[]".to_string());
            Ok(Some(json))
        }

        Action::Status => {
            let sid_detail = session_id.map_or("none".to_string(), |id| id.to_string());
            let detail = format!(
                "session_id={}, pane_count={}, focused_pane={}",
                sid_detail, runtime_vars.pane_count, runtime_vars.focused_pane,
            );
            Ok(Some(detail))
        }
    }
}

pub(super) fn require_session(session_id: Option<Uuid>) -> Result<Uuid> {
    session_id.context("no session — use new-session first")
}

pub(super) fn require_attached(attached: bool) -> Result<()> {
    if !attached {
        bail!("not attached to a session");
    }
    Ok(())
}

/// Drain output from the attached session until idle.
///
/// "Idle" is defined as `idle_threshold` consecutive empty reads separated by
/// 25ms gaps. The default threshold is 5 consecutive empty reads (125ms of
/// silence). For the `wait-for` polling loop, a lower threshold of 3 is
/// acceptable since the outer loop will re-drain on the next iteration.
///
/// Optionally captures output bytes to a display track writer for GIF export.
pub(super) async fn drain_output_until_idle(
    client: &mut BmuxClient,
    session_id: Uuid,
    max_wait: Duration,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
) -> Result<()> {
    drain_output_with_threshold(client, session_id, max_wait, display_track, 5).await
}

/// Same as `drain_output_until_idle` but with a configurable idle threshold.
pub(super) async fn drain_output_with_threshold(
    client: &mut BmuxClient,
    session_id: Uuid,
    max_wait: Duration,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    idle_threshold: u8,
) -> Result<()> {
    let started = Instant::now();
    let mut idle_polls = 0u8;

    while started.elapsed() < max_wait {
        let data = client
            .attach_output(session_id, ATTACH_OUTPUT_MAX_BYTES)
            .await
            .map_err(|e| anyhow::anyhow!("drain output failed: {e}"))?;

        if data.is_empty() {
            idle_polls += 1;
            if idle_polls >= idle_threshold {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        } else {
            if let Some(ref mut dt) = *display_track {
                let _ = dt.record_frame_bytes(&data);
            }
            idle_polls = 0;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    Ok(())
}

/// Recursively copy a directory and its contents.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("failed creating {}", dst.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("failed reading {}", src.display()))?
    {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "failed copying {} -> {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Match a server event against a user-specified event name string.
fn event_matches(event: &bmux_ipc::Event, name: &str) -> bool {
    match (event, name) {
        (bmux_ipc::Event::ServerStarted, "server_started") => true,
        (bmux_ipc::Event::ServerStopping, "server_stopping") => true,
        (bmux_ipc::Event::SessionCreated { .. }, "session_created") => true,
        (bmux_ipc::Event::SessionRemoved { .. }, "session_removed") => true,
        (bmux_ipc::Event::ClientAttached { .. }, "client_attached") => true,
        (bmux_ipc::Event::ClientDetached { .. }, "client_detached") => true,
        (bmux_ipc::Event::AttachViewChanged { .. }, "attach_view_changed") => true,
        _ => false,
    }
}
