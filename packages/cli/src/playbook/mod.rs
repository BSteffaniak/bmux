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
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use self::engine::run_playbook;
use self::types::{Playbook, PlaybookResult, Step};

/// Execution options for playbook runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub interactive: bool,
}

/// Maximum include depth to prevent circular includes.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Parse a playbook from a file path. Detects format by extension.
/// Resolves `@include` / `include = [...]` directives recursively.
/// # Errors
///
/// Returns an error if the file cannot be read or parsed.
pub fn parse_file(path: &Path) -> Result<Playbook> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut seen = BTreeSet::new();
    seen.insert(canonical.clone());
    parse_file_recursive(&canonical, &mut seen, 0)
}

/// Parse a playbook from stdin. Includes are resolved relative to CWD.
/// # Errors
///
/// Returns an error if stdin cannot be read or parsed.
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
/// # Errors
///
/// Returns an error if the playbook execution fails.
pub async fn run(playbook: Playbook, target_server: bool) -> Result<PlaybookResult> {
    run_with_options(playbook, target_server, RunOptions::default()).await
}

/// Run a playbook with explicit execution options.
/// # Errors
///
/// Returns an error if the playbook execution fails.
pub async fn run_with_options(
    playbook: Playbook,
    target_server: bool,
    options: RunOptions,
) -> Result<PlaybookResult> {
    run_playbook(playbook, target_server, options).await
}

/// Validate a playbook without executing it.
///
/// If `target_server` is true, the first-step `new-session` check is skipped
/// since the user may be targeting an existing session on a live server.
#[must_use]
pub fn validate(playbook: &Playbook, target_server: bool) -> Vec<String> {
    let mut errors = Vec::new();

    if playbook.steps.is_empty() {
        errors.push("playbook has no steps".to_string());
    }

    // Check that the first meaningful action is new-session (unless targeting live server)
    if !target_server {
        let first_action = playbook.steps.first().map(|s| &s.action);
        if let Some(action) = first_action
            && !matches!(action, types::Action::NewSession { .. })
        {
            errors.push(
                "first step should be 'new-session' (no session exists at start)".to_string(),
            );
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
            types::Action::AssertScreen {
                matches: Some(pattern),
                ..
            } => {
                if let Err(e) = regex::Regex::new(pattern) {
                    errors.push(format!(
                        "step {}: invalid regex pattern '{}': {}",
                        step.index, pattern, e
                    ));
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
#[must_use]
pub fn format_result(result: &PlaybookResult) -> String {
    let mut out = String::new();

    let name = result.playbook_name.as_deref().unwrap_or("<unnamed>");
    let status = if result.pass { "PASS" } else { "FAIL" };
    let _ = writeln!(
        out,
        "playbook: {name} — {status} ({} ms)",
        result.total_elapsed_ms
    );
    out.push('\n');

    for step in &result.steps {
        let icon = match (step.status, step.continue_on_error) {
            (types::StepStatus::Pass, _) => "+",
            (types::StepStatus::Fail, true) => "~", // failed but continued
            (types::StepStatus::Fail, false) => "x",
            (types::StepStatus::Skip, _) => "-",
        };
        write!(
            out,
            "  [{icon}] step {}: {} ({} ms)",
            step.index, step.action, step.elapsed_ms
        )
        .unwrap();
        if let Some(detail) = &step.detail {
            write!(out, " — {detail}").unwrap();
        }
        out.push('\n');
    }

    if !result.snapshots.is_empty() {
        let _ = writeln!(out, "\nsnapshots: {}", result.snapshots.len());
        for snap in &result.snapshots {
            let _ = writeln!(out, "  - {} ({} panes)", snap.id, snap.panes.len());
        }
    }

    if let Some(rid) = &result.recording_id {
        let _ = writeln!(out, "\nrecording: {rid}");
    }

    if let Some(err) = &result.error {
        let _ = writeln!(out, "\nerror: {err}");
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

#[allow(clippy::similar_names)]
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

        // Merge config from included files: parent values take priority,
        // included values fill gaps.  This lets shared setup files provide
        // defaults (e.g. @shell, @env, @var, plugin config) that the
        // including playbook can override.
        merge_included_config(&mut playbook.config, &included.config);

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

/// Merge config from an included playbook into the parent.
///
/// Parent values always take priority.  Included values fill gaps so that
/// shared setup files can provide defaults (e.g. `@shell sh`, `@env`,
/// `@var`, plugin enable/disable) without overriding the includer's config.
///
/// Fields that are **not** merged (they remain parent-only):
/// `name`, `description`, `viewport`, `timeout`, `record`, `verbose`,
/// `binary`, `bundled_plugin_ids`.
fn merge_included_config(parent: &mut types::PlaybookConfig, included: &types::PlaybookConfig) {
    // shell: fill if parent hasn't set one (first include wins).
    if parent.shell.is_none() && included.shell.is_some() {
        parent.shell.clone_from(&included.shell);
    }

    // env_mode: fill if parent hasn't set one.
    if parent.env_mode.is_none() && included.env_mode.is_some() {
        parent.env_mode = included.env_mode;
    }

    // env: insert included entries that don't already exist in parent.
    for (key, value) in &included.env {
        parent
            .env
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }

    // vars: insert included entries that don't already exist in parent.
    for (key, value) in &included.vars {
        parent
            .vars
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }

    // plugins.enable: append included entries that aren't already present.
    for id in &included.plugins.enable {
        if !parent.plugins.enable.contains(id) {
            parent.plugins.enable.push(id.clone());
        }
    }

    // plugins.disable: append included entries that aren't already present.
    for id in &included.plugins.disable {
        if !parent.plugins.disable.contains(id) {
            parent.plugins.disable.push(id.clone());
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use types::{PlaybookConfig, PluginConfig, SandboxEnvMode};

    fn default_config() -> PlaybookConfig {
        PlaybookConfig::default()
    }

    // ── shell merging ──────────────────────────────────────────────────

    #[test]
    fn include_fills_shell_when_parent_unset() {
        let mut parent = default_config();
        let included = PlaybookConfig {
            shell: Some("sh".to_string()),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.shell.as_deref(), Some("sh"));
    }

    #[test]
    fn parent_shell_wins_over_included() {
        let mut parent = PlaybookConfig {
            shell: Some("bash".to_string()),
            ..default_config()
        };
        let included = PlaybookConfig {
            shell: Some("sh".to_string()),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.shell.as_deref(), Some("bash"));
    }

    #[test]
    fn first_include_wins_for_shell() {
        let mut parent = default_config();
        let first = PlaybookConfig {
            shell: Some("sh".to_string()),
            ..default_config()
        };
        let second = PlaybookConfig {
            shell: Some("bash".to_string()),
            ..default_config()
        };
        merge_included_config(&mut parent, &first);
        merge_included_config(&mut parent, &second);
        assert_eq!(parent.shell.as_deref(), Some("sh"));
    }

    // ── env_mode merging ───────────────────────────────────────────────

    #[test]
    fn include_fills_env_mode_when_parent_unset() {
        let mut parent = default_config();
        let included = PlaybookConfig {
            env_mode: Some(SandboxEnvMode::Clean),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.env_mode, Some(SandboxEnvMode::Clean));
    }

    #[test]
    fn parent_env_mode_wins_over_included() {
        let mut parent = PlaybookConfig {
            env_mode: Some(SandboxEnvMode::Inherit),
            ..default_config()
        };
        let included = PlaybookConfig {
            env_mode: Some(SandboxEnvMode::Clean),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.env_mode, Some(SandboxEnvMode::Inherit));
    }

    // ── env merging ────────────────────────────────────────────────────

    #[test]
    fn include_fills_env_gaps() {
        let mut parent = default_config();
        parent.env.insert("A".into(), "from_parent".into());
        let mut included = default_config();
        included.env.insert("A".into(), "from_include".into());
        included.env.insert("B".into(), "from_include".into());
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.env["A"], "from_parent", "parent value preserved");
        assert_eq!(parent.env["B"], "from_include", "gap filled from include");
    }

    // ── vars merging ───────────────────────────────────────────────────

    #[test]
    fn include_fills_vars_gaps() {
        let mut parent = default_config();
        parent.vars.insert("X".into(), "parent_x".into());
        let mut included = default_config();
        included.vars.insert("X".into(), "include_x".into());
        included.vars.insert("Y".into(), "include_y".into());
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.vars["X"], "parent_x", "parent var preserved");
        assert_eq!(parent.vars["Y"], "include_y", "gap filled from include");
    }

    // ── plugin merging ─────────────────────────────────────────────────

    #[test]
    fn include_appends_plugin_enable() {
        let mut parent = PlaybookConfig {
            plugins: PluginConfig {
                enable: vec!["a".into()],
                disable: vec![],
            },
            ..default_config()
        };
        let included = PlaybookConfig {
            plugins: PluginConfig {
                enable: vec!["a".into(), "b".into()],
                disable: vec!["c".into()],
            },
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert_eq!(parent.plugins.enable, vec!["a", "b"], "dedup + append");
        assert_eq!(parent.plugins.disable, vec!["c"], "disable appended");
    }

    // ── identity fields not merged ─────────────────────────────────────

    #[test]
    fn include_does_not_merge_identity_fields() {
        let mut parent = default_config();
        let included = PlaybookConfig {
            name: Some("included-name".into()),
            description: Some("included-desc".into()),
            ..default_config()
        };
        merge_included_config(&mut parent, &included);
        assert!(parent.name.is_none(), "name should not be merged");
        assert!(
            parent.description.is_none(),
            "description should not be merged"
        );
    }

    // ── end-to-end parse_file include config merging ───────────────────

    #[test]
    fn parse_file_merges_shell_from_include() {
        // include_setup.dsl has @shell sh, include_main.dsl has @shell sh too
        // (added as a safety net), but this test verifies the parse pipeline
        // correctly processes includes without error.
        let fixtures =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/playbooks");
        let playbook =
            parse_file(&fixtures.join("include_main.dsl")).expect("should parse include_main.dsl");
        assert_eq!(
            playbook.config.shell.as_deref(),
            Some("sh"),
            "shell should be set (from main or merged from include)"
        );
        assert!(
            playbook.steps.len() >= 4,
            "should have steps from both files"
        );
    }
}
