use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::parse_dsl::decode_c_escapes;
use super::types::{
    Action, Playbook, PlaybookConfig, PlaybookRenderRowRef, PlaybookRenderRowSegmentRef,
    PlaybookRenderTraceOp, PluginConfig, RenderAssertion, ServiceKind, SplitDirection, Step,
    Viewport,
};

/// Parse a playbook from a TOML string.
///
/// Returns the playbook and a list of include paths that the caller
/// is responsible for resolving and merging.
/// # Errors
///
/// Returns an error if the TOML input is malformed or contains invalid fields.
pub fn parse_toml(input: &str) -> Result<(Playbook, Vec<(usize, String)>)> {
    let raw: RawPlaybook = toml::from_str(input).context("invalid playbook TOML")?;
    let includes: Vec<(usize, String)> = raw
        .playbook
        .as_ref()
        .and_then(|p| p.include.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|path| (0, path)) // TOML includes are always at position 0 (before all steps)
        .collect();
    let config = parse_config(raw.playbook)?;
    let steps = raw
        .step
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, raw_step)| {
            let coe = raw_step.continue_on_error.unwrap_or(false);
            let action =
                parse_step_action(raw_step).with_context(|| format!("step {}: invalid", i + 1))?;
            Ok(Step {
                index: i,
                action,
                continue_on_error: coe,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((Playbook { config, steps }, includes))
}

fn parse_config(raw: Option<RawPlaybookConfig>) -> Result<PlaybookConfig> {
    let Some(raw) = raw else {
        return Ok(PlaybookConfig::default());
    };

    let viewport = raw.viewport.map_or_else(Viewport::default, |v| {
        let defaults = Viewport::default();
        Viewport {
            cols: v.cols.unwrap_or(defaults.cols),
            rows: v.rows.unwrap_or(defaults.rows),
        }
    });

    let timeout = raw
        .timeout_ms
        .map_or(Duration::from_secs(30), Duration::from_millis);

    let plugins = match raw.plugins {
        Some(p) => PluginConfig {
            enable: p.enable.unwrap_or_default(),
            disable: p.disable.unwrap_or_default(),
        },
        None => PluginConfig::default(),
    };

    let env_mode = match raw.env_mode.as_deref() {
        Some("clean") => Some(super::types::SandboxEnvMode::Clean),
        Some("inherit") => Some(super::types::SandboxEnvMode::Inherit),
        None => None,
        Some(other) => bail!("env_mode must be 'inherit' or 'clean', got '{other}'"),
    };

    Ok(PlaybookConfig {
        name: raw.name,
        description: raw.description,
        viewport,
        shell: raw.shell,
        timeout,
        record: raw.record.unwrap_or(false),
        plugins,
        vars: raw.vars.unwrap_or_default(),
        env: raw.env.unwrap_or_default(),
        env_mode,
        binary: None,
        bundled_plugin_ids: Vec::new(),
        verbose: false,
        render_trace: raw.render_trace.unwrap_or(false),
        driver: super::types::PlaybookDriver::Sandbox,
    })
}

#[allow(clippy::too_many_lines)]
fn parse_step_action(step: RawStep) -> Result<Action> {
    match step.action.as_str() {
        "new-session" => Ok(Action::NewSession { name: step.name }),
        "kill-session" => {
            let name = step.name.context("kill-session requires 'name'")?;
            Ok(Action::KillSession { name })
        }
        "split-pane" => {
            let direction = match step.direction.as_deref() {
                Some("vertical" | "v") | None => SplitDirection::Vertical,
                Some("horizontal" | "h") => SplitDirection::Horizontal,
                Some(other) => bail!("invalid split direction: {other}"),
            };
            Ok(Action::SplitPane {
                direction,
                ratio: step.ratio,
            })
        }
        "focus-pane" => {
            let target = step.target.context("focus-pane requires 'target'")?;
            Ok(Action::FocusPane { target })
        }
        "close-pane" => Ok(Action::ClosePane {
            target: step.target,
        }),
        "send-keys" => {
            let raw = step.keys.context("send-keys requires 'keys'")?;
            let bytes = decode_c_escapes(&raw)?;
            Ok(Action::SendKeys {
                keys: bytes,
                pane: step.pane,
            })
        }
        "send-bytes" => {
            let hex_str = step.hex.context("send-bytes requires 'hex'")?;
            let bytes = decode_hex(&hex_str)?;
            Ok(Action::SendBytes { hex: bytes })
        }
        "wait-for" => {
            let pattern = step.pattern.context("wait-for requires 'pattern'")?;
            let timeout_ms = step.timeout_ms.unwrap_or(5000);
            Ok(Action::WaitFor {
                pattern,
                pane: step.pane,
                timeout: Duration::from_millis(timeout_ms),
                retry: step.retry.unwrap_or(1),
            })
        }
        "sleep" => {
            let ms = step.ms.context("sleep requires 'ms'")?;
            Ok(Action::Sleep {
                duration: Duration::from_millis(ms),
            })
        }
        "snapshot" => {
            let id = step.id.context("snapshot requires 'id'")?;
            Ok(Action::Snapshot { id })
        }
        "assert-screen" => {
            if step.contains.is_none() && step.not_contains.is_none() && step.matches.is_none() {
                bail!("assert-screen requires at least one of: contains, not_contains, matches");
            }
            Ok(Action::AssertScreen {
                pane: step.pane,
                contains: step.contains,
                not_contains: step.not_contains,
                matches: step.matches,
                scrollback: step.scrollback.unwrap_or(false),
            })
        }
        "assert-layout" => {
            let pane_count = step
                .pane_count
                .context("assert-layout requires pane_count")?;
            Ok(Action::AssertLayout { pane_count })
        }
        "assert-cursor" => {
            let row = step.row.context("assert-cursor requires 'row'")?;
            let col = step.col.context("assert-cursor requires 'col'")?;
            Ok(Action::AssertCursor {
                pane: step.pane,
                row,
                col,
            })
        }
        "resize-viewport" => {
            let cols = step.cols.context("resize-viewport requires 'cols'")?;
            let rows = step.rows.context("resize-viewport requires 'rows'")?;
            Ok(Action::ResizeViewport { cols, rows })
        }
        "send-attach" => {
            let key = step.key.context("send-attach requires 'key'")?;
            Ok(Action::SendAttach { key })
        }
        "seed-pane-text" => {
            let lines = step.lines.context("seed-pane-text requires 'lines'")?;
            let cursor_row = step
                .cursor_row
                .context("seed-pane-text requires 'cursor_row'")?;
            let cursor_col = step
                .cursor_col
                .context("seed-pane-text requires 'cursor_col'")?;
            Ok(Action::SeedPaneText {
                lines,
                cursor_row,
                cursor_col,
            })
        }
        "prefix-key" => {
            let key_str = step.key.context("prefix-key requires 'key'")?;
            let key = key_str.chars().next().context("empty key")?;
            Ok(Action::PrefixKey { key })
        }
        "wait-for-event" => {
            let event = step.event.context("wait-for-event requires 'event'")?;
            let timeout_ms = step.timeout_ms.unwrap_or(5000);
            Ok(Action::WaitForEvent {
                event,
                timeout: Duration::from_millis(timeout_ms),
            })
        }
        "invoke-service" => {
            let capability = step
                .capability
                .context("invoke-service requires 'capability'")?;
            let kind = match step.kind.as_deref() {
                Some("query" | "q") => ServiceKind::Query,
                Some("command" | "cmd") | None => ServiceKind::Command,
                Some(other) => bail!("invalid service kind: {other}"),
            };
            let interface_id = step
                .interface
                .context("invoke-service requires 'interface'")?;
            let operation = step
                .operation
                .context("invoke-service requires 'operation'")?;
            let payload = step.payload.unwrap_or_default();
            Ok(Action::InvokeService {
                capability,
                kind,
                interface_id,
                operation,
                payload,
            })
        }
        "screen" => Ok(Action::Screen),
        "status" => Ok(Action::Status),
        "render-mark" => {
            let id = step.id.context("render-mark requires 'id'")?;
            Ok(Action::RenderMark { id })
        }
        "assert-render" => {
            let since = step.since.context("assert-render requires 'since'")?;
            Ok(Action::AssertRender {
                since,
                assertion: RenderAssertion {
                    min_frames: step.min_frames,
                    max_frames: step.max_frames,
                    full_frame: step.full_frame,
                    max_full_frame_frames: step.max_full_frame_frames,
                    max_full_surface_fallbacks: step.max_full_surface_fallbacks,
                    max_damage_rects: step.max_damage_rects,
                    max_damage_area_cells: step.max_damage_area_cells,
                    max_rows_emitted: step.max_rows_emitted,
                    max_row_segments_emitted: step.max_row_segments_emitted,
                    max_cells_emitted: step.max_cells_emitted,
                    max_frame_bytes: step.max_frame_bytes,
                    status_rendered: step.status_rendered,
                    overlay_rendered: step.overlay_rendered,
                    expected_emitted_rows: step
                        .expected_emitted_rows
                        .as_deref()
                        .map(parse_render_row_refs_toml)
                        .transpose()?,
                    expected_emitted_row_segments: step
                        .expected_emitted_row_segments
                        .as_deref()
                        .map(parse_render_row_segment_refs_toml)
                        .transpose()?,
                    expected_trace_ops: step
                        .expected_trace_ops
                        .as_deref()
                        .map(parse_render_trace_ops_toml)
                        .transpose()?,
                },
            })
        }
        other => bail!("unknown action: {other}"),
    }
}

fn parse_render_row_refs_toml(rows: &[String]) -> Result<Vec<PlaybookRenderRowRef>> {
    rows.iter()
        .map(|entry| {
            let (pane, row) = entry
                .split_once(':')
                .with_context(|| format!("invalid render row ref '{entry}', expected pane:row"))?;
            Ok(PlaybookRenderRowRef {
                pane: pane.parse().context("invalid render row pane")?,
                row: row.parse().context("invalid render row index")?,
            })
        })
        .collect()
}

fn parse_render_row_segment_refs_toml(
    segments: &[String],
) -> Result<Vec<PlaybookRenderRowSegmentRef>> {
    segments
        .iter()
        .map(|entry| {
            let parts = entry.split(':').collect::<Vec<_>>();
            if parts.len() != 4 {
                bail!(
                    "invalid render row segment ref '{entry}', expected pane:row:start_col:cells"
                );
            }
            Ok(PlaybookRenderRowSegmentRef {
                pane: parts[0]
                    .parse()
                    .context("invalid render row segment pane")?,
                row: parts[1].parse().context("invalid render row segment row")?,
                start_col: parts[2]
                    .parse()
                    .context("invalid render row segment start_col")?,
                cells: parts[3]
                    .parse()
                    .context("invalid render row segment cells")?,
            })
        })
        .collect()
}

fn parse_render_trace_ops_toml(ops: &[String]) -> Result<Vec<PlaybookRenderTraceOp>> {
    ops.iter()
        .map(|entry| PlaybookRenderTraceOp::parse_compact(entry).map_err(anyhow::Error::msg))
        .collect()
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        bail!("hex string must have even length");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("invalid hex at position {i}"))
        })
        .collect()
}

// ── Raw TOML deserialization types ───────────────────────────────────────────

#[derive(Deserialize)]
struct RawPlaybook {
    playbook: Option<RawPlaybookConfig>,
    step: Option<Vec<RawStep>>,
}

#[derive(Deserialize)]
struct RawPlaybookConfig {
    name: Option<String>,
    description: Option<String>,
    viewport: Option<RawViewport>,
    shell: Option<String>,
    timeout_ms: Option<u64>,
    record: Option<bool>,
    render_trace: Option<bool>,
    plugins: Option<RawPluginConfig>,
    vars: Option<BTreeMap<String, String>>,
    env: Option<BTreeMap<String, String>>,
    env_mode: Option<String>,
    include: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawViewport {
    cols: Option<u16>,
    rows: Option<u16>,
}

#[derive(Deserialize)]
struct RawPluginConfig {
    enable: Option<Vec<String>>,
    disable: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawStep {
    action: String,
    continue_on_error: Option<bool>,
    // Session/pane management
    name: Option<String>,
    direction: Option<String>,
    ratio: Option<f64>,
    target: Option<u32>,
    // Input
    keys: Option<String>,
    hex: Option<String>,
    key: Option<String>,
    pane: Option<u32>,
    lines: Option<Vec<String>>,
    cursor_row: Option<u16>,
    cursor_col: Option<u16>,
    // Waiting
    pattern: Option<String>,
    timeout_ms: Option<u64>,
    retry: Option<u32>,
    ms: Option<u64>,
    // Snapshot
    id: Option<String>,
    // Assertions
    contains: Option<String>,
    not_contains: Option<String>,
    matches: Option<String>,
    scrollback: Option<bool>,
    pane_count: Option<u32>,
    row: Option<u16>,
    col: Option<u16>,
    // Resize
    cols: Option<u16>,
    rows: Option<u16>,
    // Events
    event: Option<String>,
    // Service invocation
    capability: Option<String>,
    kind: Option<String>,
    interface: Option<String>,
    operation: Option<String>,
    payload: Option<String>,
    // Render assertions
    since: Option<String>,
    min_frames: Option<u64>,
    max_frames: Option<u64>,
    full_frame: Option<bool>,
    max_full_frame_frames: Option<u64>,
    max_full_surface_fallbacks: Option<u64>,
    max_damage_rects: Option<u64>,
    max_damage_area_cells: Option<u64>,
    max_rows_emitted: Option<u64>,
    max_row_segments_emitted: Option<u64>,
    max_cells_emitted: Option<u64>,
    max_frame_bytes: Option<u64>,
    status_rendered: Option<bool>,
    overlay_rendered: Option<bool>,
    expected_emitted_rows: Option<Vec<String>>,
    expected_emitted_row_segments: Option<Vec<String>>,
    expected_trace_ops: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_toml() {
        let input = r#"
[[step]]
action = "new-session"
name = "main"

[[step]]
action = "send-keys"
keys = "echo hello\r"
"#;
        let (playbook, _includes) = parse_toml(input).unwrap();
        assert_eq!(playbook.steps.len(), 2);
        assert_eq!(playbook.steps[0].action.name(), "new-session");
        assert_eq!(playbook.steps[1].action.name(), "send-keys");
    }

    #[test]
    fn parse_full_toml() {
        let input = r#"
[playbook]
name = "test"
description = "A test playbook"
viewport = { cols = 120, rows = 50 }
shell = "/bin/bash"
timeout_ms = 15000
record = true

[playbook.plugins]
enable = ["bmux.windows"]

[[step]]
action = "new-session"
name = "main"

[[step]]
action = "split-pane"
direction = "vertical"
ratio = 0.5

[[step]]
action = "send-keys"
keys = "echo hello\\r"

[[step]]
action = "wait-for"
pattern = "hello"
timeout_ms = 3000

[[step]]
action = "assert-screen"
pane = 0
contains = "hello"
"#;
        let (playbook, _includes) = parse_toml(input).unwrap();
        assert_eq!(playbook.config.name.as_deref(), Some("test"));
        assert_eq!(playbook.config.viewport.cols, 120);
        assert_eq!(playbook.config.viewport.rows, 50);
        assert_eq!(playbook.config.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(playbook.config.timeout, Duration::from_secs(15));
        assert!(playbook.config.record);
        assert_eq!(playbook.config.plugins.enable, vec!["bmux.windows"]);
        assert_eq!(playbook.steps.len(), 5);
    }

    #[test]
    fn parse_assert_render_trace_ops() {
        let input = r#"
[playbook]
render_trace = true

[[step]]
action = "render-mark"
id = "baseline"

[[step]]
action = "assert-render"
since = "baseline"
expected_trace_ops = ["full-frame", "pane-row-segment:1:2:3:4", "prompt-overlay", "cursor:1:true"]
"#;
        let (playbook, _includes) = parse_toml(input).unwrap();
        match &playbook.steps[1].action {
            Action::AssertRender { assertion, .. } => assert_eq!(
                assertion.expected_trace_ops,
                Some(vec![
                    PlaybookRenderTraceOp::FullFrame,
                    PlaybookRenderTraceOp::PaneRowSegment {
                        pane: 1,
                        row: 2,
                        start_col: 3,
                        cells: 4,
                    },
                    PlaybookRenderTraceOp::PromptOverlay,
                    PlaybookRenderTraceOp::Cursor {
                        pane: 1,
                        visible: true,
                    },
                ])
            ),
            other => panic!("expected assert-render, got {other:?}"),
        }
    }

    #[test]
    fn parse_toml_defaults() {
        let input = r#"
[[step]]
action = "sleep"
ms = 100
"#;
        let (playbook, _includes) = parse_toml(input).unwrap();
        assert!(playbook.config.name.is_none());
        assert_eq!(playbook.config.viewport.cols, 80);
        assert_eq!(playbook.config.viewport.rows, 24);
        assert_eq!(playbook.config.timeout, Duration::from_secs(30));
        assert!(!playbook.config.record);
    }

    #[test]
    fn toml_unknown_action_fails() {
        let input = r#"
[[step]]
action = "nonexistent"
"#;
        assert!(parse_toml(input).is_err());
    }

    #[test]
    fn toml_split_pane_defaults() {
        let input = r#"
[[step]]
action = "split-pane"
"#;
        let (playbook, _includes) = parse_toml(input).unwrap();
        match &playbook.steps[0].action {
            Action::SplitPane { direction, ratio } => {
                assert_eq!(*direction, SplitDirection::Vertical);
                assert!(ratio.is_none());
            }
            _ => panic!("expected split-pane"),
        }
    }

    #[test]
    fn toml_empty_playbook() {
        let input = "";
        let (playbook, _includes) = parse_toml(input).unwrap();
        assert!(playbook.steps.is_empty());
    }

    #[test]
    fn parse_toml_env_mode_clean() {
        let input = r#"
[playbook]
env_mode = "clean"

[[step]]
action = "new-session"
"#;
        let (playbook, _) = parse_toml(input).unwrap();
        assert_eq!(
            playbook.config.env_mode,
            Some(super::super::types::SandboxEnvMode::Clean)
        );
    }

    #[test]
    fn parse_toml_env_mode_absent() {
        let input = r#"
[[step]]
action = "new-session"
"#;
        let (playbook, _) = parse_toml(input).unwrap();
        assert!(playbook.config.env_mode.is_none());
    }

    #[test]
    fn parse_toml_env_mode_invalid_fails() {
        let input = r#"
[playbook]
env_mode = "garbage"

[[step]]
action = "new-session"
"#;
        assert!(parse_toml(input).is_err());
    }
}
