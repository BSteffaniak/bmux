//! Playbook system for headless scripted bmux execution.
//!
//! Supports two input formats:
//! - TOML playbook files (`.playbook.toml`)
//! - Line-oriented DSL (pipeable from stdin)
//!
//! Both formats parse into the same internal representation and are executed
//! by the same engine against an ephemeral sandbox server (or a live server).

pub mod diff;
pub mod display_track;
pub mod engine;
pub mod from_recording;
pub mod interactive;
pub mod parse_dsl;
pub mod parse_toml;
pub mod sandbox;
pub mod screen;
pub mod subst;
pub mod types;

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use self::engine::run_playbook;
use self::types::{Playbook, PlaybookResult, Step};

/// Maximum include depth to prevent circular includes.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Parse a playbook from a file path. Detects format by extension.
/// Resolves `@include` / `include = [...]` directives recursively.
pub fn parse_file(path: &Path) -> Result<Playbook> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut seen = BTreeSet::new();
    seen.insert(canonical.clone());
    parse_file_recursive(&canonical, &mut seen, 0)
}

/// Parse a playbook from stdin. Includes are resolved relative to CWD.
pub fn parse_stdin() -> Result<Playbook> {
    let mut content = String::new();
    std::io::stdin()
        .read_to_string(&mut content)
        .context("failed reading stdin")?;

    let (mut playbook, includes) = parse_content(&content)?;

    // Resolve includes relative to CWD
    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut seen = BTreeSet::new();
    resolve_includes(&mut playbook, &includes, &base_dir, &mut seen, 0)?;

    Ok(playbook)
}

/// Run a playbook and return the result.
pub async fn run(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    run_playbook(playbook, target_server).await
}

/// Validate a playbook without executing it.
///
/// If `target_server` is true, the first-step `new-session` check is skipped
/// since the user may be targeting an existing session on a live server.
pub fn validate(playbook: &Playbook, target_server: bool) -> Vec<String> {
    let mut errors = Vec::new();

    if playbook.steps.is_empty() {
        errors.push("playbook has no steps".to_string());
    }

    // Check that the first meaningful action is new-session (unless targeting live server)
    if !target_server {
        let first_action = playbook.steps.first().map(|s| &s.action);
        if let Some(action) = first_action {
            if !matches!(action, types::Action::NewSession { .. }) {
                errors.push(
                    "first step should be 'new-session' (no session exists at start)".to_string(),
                );
            }
        }
    }

    // Check viewport dimensions
    if playbook.config.viewport.cols < 10 || playbook.config.viewport.rows < 5 {
        errors.push("viewport too small (minimum 10x5)".to_string());
    }

    // Track expected pane count for index validation
    let mut expected_pane_count: u32 = 0;

    for step in &playbook.steps {
        match &step.action {
            types::Action::NewSession { .. } => {
                expected_pane_count = 1;
            }
            types::Action::SplitPane { .. } => {
                expected_pane_count += 1;
            }
            types::Action::ClosePane { .. } => {
                expected_pane_count = expected_pane_count.saturating_sub(1);
            }

            // Validate regex patterns
            types::Action::WaitFor {
                pattern, timeout, ..
            } => {
                if timeout.is_zero() {
                    errors.push(format!("step {}: wait-for has zero timeout", step.index));
                }
                if let Err(e) = regex::Regex::new(pattern) {
                    errors.push(format!(
                        "step {}: invalid regex pattern '{}': {}",
                        step.index, pattern, e
                    ));
                }
            }
            types::Action::AssertScreen { matches, .. } => {
                if let Some(pattern) = matches {
                    if let Err(e) = regex::Regex::new(pattern) {
                        errors.push(format!(
                            "step {}: invalid regex pattern '{}': {}",
                            step.index, pattern, e
                        ));
                    }
                }
            }

            // Validate event names
            types::Action::WaitForEvent { event, timeout } => {
                if timeout.is_zero() {
                    errors.push(format!(
                        "step {}: wait-for-event has zero timeout",
                        step.index
                    ));
                }
                let valid_events = [
                    "server_started",
                    "server_stopping",
                    "session_created",
                    "session_removed",
                    "client_attached",
                    "client_detached",
                    "attach_view_changed",
                ];
                if !valid_events.contains(&event.as_str()) {
                    errors.push(format!(
                        "step {}: unknown event '{}'; valid events: {}",
                        step.index,
                        event,
                        valid_events.join(", ")
                    ));
                }
            }

            // Warn if targeting a pane before any split
            types::Action::SendKeys {
                pane: Some(idx), ..
            }
            | types::Action::FocusPane { target: idx } => {
                if expected_pane_count < 2 && *idx > 1 {
                    errors.push(format!(
                        "step {}: targets pane {} but only {} pane(s) expected at this point",
                        step.index, idx, expected_pane_count
                    ));
                }
            }

            _ => {}
        }
    }

    errors
}

/// Format a `PlaybookResult` as human-readable output.
pub fn format_result(result: &PlaybookResult) -> String {
    let mut out = String::new();

    let name = result.playbook_name.as_deref().unwrap_or("<unnamed>");
    let status = if result.pass { "PASS" } else { "FAIL" };
    out.push_str(&format!(
        "playbook: {name} — {status} ({} ms)\n",
        result.total_elapsed_ms
    ));
    out.push('\n');

    for step in &result.steps {
        let icon = match (step.status, step.continue_on_error) {
            (types::StepStatus::Pass, _) => "+",
            (types::StepStatus::Fail, true) => "~", // failed but continued
            (types::StepStatus::Fail, false) => "x",
            (types::StepStatus::Skip, _) => "-",
        };
        out.push_str(&format!(
            "  [{icon}] step {}: {} ({} ms)",
            step.index, step.action, step.elapsed_ms
        ));
        if let Some(detail) = &step.detail {
            out.push_str(&format!(" — {detail}"));
        }
        out.push('\n');
    }

    if !result.snapshots.is_empty() {
        out.push_str(&format!("\nsnapshots: {}\n", result.snapshots.len()));
        for snap in &result.snapshots {
            out.push_str(&format!("  - {} ({} panes)\n", snap.id, snap.panes.len()));
        }
    }

    if let Some(rid) = &result.recording_id {
        out.push_str(&format!("\nrecording: {rid}\n"));
    }

    if let Some(err) = &result.error {
        out.push_str(&format!("\nerror: {err}\n"));
    }

    out
}

// ── Include resolution ───────────────────────────────────────────────────────

fn parse_file_recursive(
    path: &Path,
    seen: &mut BTreeSet<PathBuf>,
    depth: usize,
) -> Result<Playbook> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!("include depth exceeds maximum ({MAX_INCLUDE_DEPTH})");
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;

    let (mut playbook, includes) =
        parse_content(&content).with_context(|| format!("failed parsing {}", path.display()))?;

    let base_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    resolve_includes(&mut playbook, &includes, &base_dir, seen, depth)?;

    Ok(playbook)
}

fn resolve_includes(
    playbook: &mut Playbook,
    includes: &[(usize, String)],
    base_dir: &Path,
    seen: &mut BTreeSet<PathBuf>,
    depth: usize,
) -> Result<()> {
    if includes.is_empty() {
        return Ok(());
    }

    // Resolve all includes and collect their steps with insertion positions.
    let mut insertions: Vec<(usize, Vec<Step>)> = Vec::new();

    for (insert_at, include_path) in includes {
        let resolved = base_dir.join(include_path);
        let canonical = resolved
            .canonicalize()
            .with_context(|| format!("include path not found: {}", resolved.display()))?;

        if !seen.insert(canonical.clone()) {
            bail!(
                "circular include detected: {} already included",
                canonical.display()
            );
        }

        let included = parse_file_recursive(&canonical, seen, depth + 1)
            .with_context(|| format!("failed parsing included file {}", canonical.display()))?;

        // Only merge steps from included files; config is ignored.
        insertions.push((*insert_at, included.steps));
    }

    // Insert included steps at their declared positions (in reverse order
    // to preserve indices as we insert).
    insertions.sort_by(|a, b| b.0.cmp(&a.0)); // reverse sort by position
    for (insert_at, steps) in insertions {
        let pos = insert_at.min(playbook.steps.len());
        for (i, step) in steps.into_iter().enumerate() {
            playbook.steps.insert(pos + i, step);
        }
    }

    // Re-index all steps sequentially.
    for (i, step) in playbook.steps.iter_mut().enumerate() {
        step.index = i;
    }

    Ok(())
}

/// Parse raw content, trying TOML first then DSL.
fn parse_content(content: &str) -> Result<(Playbook, Vec<(usize, String)>)> {
    // Try TOML first
    if let Ok(result) = parse_toml::parse_toml(content) {
        return Ok(result);
    }
    // Fall back to DSL
    parse_dsl::parse_dsl(content)
}
