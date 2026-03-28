//! Playbook execution engine.
//!
//! Orchestrates the full lifecycle: parse → sandbox → execute steps → report.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bmux_client::BmuxClient;
use bmux_ipc::{PaneSplitDirection, SessionSelector};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::sandbox::SandboxServer;
use super::screen::ScreenInspector;
use super::types::{
    Action, Playbook, PlaybookResult, SnapshotCapture, SplitDirection, Step, StepResult, StepStatus,
};

/// Default timeout for waiting for the sandbox server to start.
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Max bytes to read from attach output per drain cycle.
const ATTACH_OUTPUT_MAX_BYTES: usize = 256 * 1024;

/// Run a playbook to completion, returning the result.
pub async fn run_playbook(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    let started = Instant::now();
    let playbook_name = playbook.config.name.clone();

    let mut step_results = Vec::new();
    let mut snapshots = Vec::new();
    let mut error_msg: Option<String> = None;

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
        )
        .await
        .context("failed starting sandbox server")?;
        client = sb.connect("bmux-playbook-runner").await?;
        sandbox = Some(sb);
    }

    let mut inspector =
        ScreenInspector::new(playbook.config.viewport.cols, playbook.config.viewport.rows);

    // Session tracking
    let mut session_id: Option<Uuid> = None;
    let mut attached = false;

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
            &playbook.config.viewport.cols,
            &playbook.config.viewport.rows,
            &mut snapshots,
            deadline,
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
        recording_id: None,
        total_elapsed_ms,
        error: error_msg,
    })
}

/// Execute a single step.
#[allow(clippy::too_many_arguments)]
async fn execute_step(
    step: &Step,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    viewport_cols: &u16,
    viewport_rows: &u16,
    snapshots: &mut Vec<SnapshotCapture>,
    deadline: Instant,
) -> Result<Option<String>> {
    match &step.action {
        Action::NewSession { name } => {
            let sid = client
                .new_session(name.clone())
                .await
                .map_err(|e| anyhow::anyhow!("new-session failed: {e}"))?;
            debug!("created session {sid}");

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
            drain_output_until_idle(client, sid, Duration::from_millis(500)).await?;

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
            drain_output_until_idle(client, sid, Duration::from_millis(300)).await?;

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
            Ok(None)
        }

        Action::SendKeys { keys, pane } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            if let Some(target_index) = pane {
                // Pane-targeted send: focus the target pane, send input, then
                // restore focus to the original pane.
                let snapshot = client
                    .attach_snapshot(sid, 0)
                    .await
                    .map_err(|e| anyhow::anyhow!("snapshot for focus check failed: {e}"))?;
                let current_focused = snapshot.panes.iter().find(|p| p.focused).map(|p| p.index);

                let target_selector = bmux_ipc::PaneSelector::ByIndex(*target_index);
                client
                    .focus_pane_target(Some(SessionSelector::ById(sid)), target_selector)
                    .await
                    .map_err(|e| anyhow::anyhow!("send-keys focus target pane failed: {e}"))?;

                client
                    .attach_input(sid, keys.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("send-keys failed: {e}"))?;

                // Restore focus to the original pane if it was different.
                if let Some(orig_index) = current_focused {
                    if orig_index != *target_index {
                        let restore_selector = bmux_ipc::PaneSelector::ByIndex(orig_index);
                        client
                            .focus_pane_target(Some(SessionSelector::ById(sid)), restore_selector)
                            .await
                            .map_err(|e| anyhow::anyhow!("send-keys restore focus failed: {e}"))?;
                    }
                }
            } else {
                client
                    .attach_input(sid, keys.clone())
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

            let wait_deadline = Instant::now() + (*timeout).min(deadline - Instant::now());
            let mut poll_delay = Duration::from_millis(10);

            loop {
                // Drain any pending output
                drain_output_until_idle(client, sid, Duration::from_millis(100)).await?;

                // Refresh screen state
                let snapshot = inspector.refresh(client, sid).await?;
                let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

                if inspector.pane_matches(pane_idx, pattern)? {
                    return Ok(Some(format!("matched pattern '{pattern}'")));
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
                        "wait-for timed out after {}ms waiting for pattern '{}'; screen: {truncated}",
                        timeout.as_millis(),
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

            drain_output_until_idle(client, sid, Duration::from_millis(200)).await?;
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

            drain_output_until_idle(client, sid, Duration::from_millis(200)).await?;
            let snapshot = inspector.refresh(client, sid).await?;
            let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

            if let Some(needle) = contains {
                if !inspector.pane_contains(pane_idx, needle) {
                    let text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    bail!(
                        "assert-screen: pane {pane_idx} does not contain '{needle}'; screen: {text}"
                    );
                }
            }

            if let Some(needle) = not_contains {
                if inspector.pane_contains(pane_idx, needle) {
                    bail!("assert-screen: pane {pane_idx} unexpectedly contains '{needle}'");
                }
            }

            if let Some(pattern) = matches {
                if !inspector.pane_matches(pane_idx, pattern)? {
                    let text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    bail!(
                        "assert-screen: pane {pane_idx} does not match '{pattern}'; screen: {text}"
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
    }
}

fn require_session(session_id: Option<Uuid>) -> Result<Uuid> {
    session_id.context("no session — use new-session first")
}

fn require_attached(attached: bool) -> Result<()> {
    if !attached {
        bail!("not attached to a session");
    }
    Ok(())
}

/// Drain output from the attached session until idle (3 consecutive empty reads).
async fn drain_output_until_idle(
    client: &mut BmuxClient,
    session_id: Uuid,
    max_wait: Duration,
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
            if idle_polls >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        } else {
            idle_polls = 0;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    Ok(())
}
