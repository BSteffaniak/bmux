//! Line-oriented DSL parser for playbooks.
//!
//! Parses the text DSL format into the shared `Playbook` representation.
//! See `docs/playbooks.md` for the full DSL syntax reference.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::types::{
    Action, Playbook, PlaybookConfig, PlaybookDriver, PlaybookRenderRowRef,
    PlaybookRenderRowSegmentRef, PlaybookRenderTraceOp, RenderAssertion, ServiceKind,
    SimTerminalEvent, SplitDirection, Step,
};

/// Parse a playbook from the line-oriented DSL format.
///
/// Each line is one of:
/// - Empty or whitespace-only: ignored
/// - Starting with `#`: comment, ignored
/// - Starting with `@`: config directive
/// - Otherwise: an action line with `key=value` arguments
///
/// Returns the playbook and a list of (`step_index`, `include_path`) pairs from
/// `@include` directives that the caller is responsible for resolving.
///
/// # Errors
///
/// Returns an error if parsing any line fails.
pub fn parse_dsl(input: &str) -> Result<(Playbook, Vec<(usize, String)>)> {
    let mut config = PlaybookConfig::default();
    let mut steps = Vec::new();
    let mut step_index = 0_usize;
    let mut includes: Vec<(usize, String)> = Vec::new();

    for (line_num, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line_ctx = line_num + 1;

        if let Some(directive) = line.strip_prefix('@') {
            parse_config_directive(directive.trim(), &mut config, &mut includes, step_index)
                .with_context(|| format!("line {line_ctx}: invalid config directive"))?;
        } else {
            // Check for !continue suffix (must be preceded by whitespace).
            let (action_line, continue_on_error) = line
                .strip_suffix(" !continue")
                .map_or((line, false), |stripped| (stripped.trim(), true));
            let action = parse_action_line(action_line)
                .with_context(|| format!("line {line_ctx}: invalid action"))?;
            steps.push(Step {
                index: step_index,
                action,
                continue_on_error,
            });
            step_index += 1;
        }
    }

    Ok((Playbook { config, steps }, includes))
}

fn parse_config_directive(
    directive: &str,
    config: &mut PlaybookConfig,
    includes: &mut Vec<(usize, String)>,
    current_step_index: usize,
) -> Result<()> {
    let (name, rest) = split_first_token(directive);
    match name {
        "viewport" => {
            let args = parse_kv_args(rest)?;
            if let Some(cols) = args.get("cols") {
                config.viewport.cols = cols.parse().context("invalid cols")?;
            }
            if let Some(rows) = args.get("rows") {
                config.viewport.rows = rows.parse().context("invalid rows")?;
            }
        }
        "shell" => {
            config.shell = Some(rest.trim().to_string());
        }
        "timeout" => {
            let ms: u64 = rest.trim().parse().context("invalid timeout ms")?;
            config.timeout = Duration::from_millis(ms);
        }
        "record" => {
            config.record = rest.trim().parse::<bool>().unwrap_or(true);
        }
        "render-trace" => {
            config.render_trace = rest.trim().parse::<bool>().unwrap_or(true);
        }
        "driver" => {
            config.driver = match rest.trim() {
                "sandbox" => PlaybookDriver::Sandbox,
                "real-attach" => PlaybookDriver::RealAttach,
                "attach-sim" => PlaybookDriver::AttachSim,
                other => bail!(
                    "@driver must be 'sandbox', 'real-attach', or 'attach-sim', got '{other}'"
                ),
            };
        }
        "sandbox-config" => parse_sandbox_config_directive(rest, config)?,
        "name" => {
            config.name = Some(rest.trim().to_string());
        }
        "description" => {
            config.description = Some(rest.trim().to_string());
        }
        "plugin" => {
            let args = parse_kv_args(rest)?;
            if let Some(enable) = args.get("enable") {
                config.plugins.enable.push(enable.clone());
            }
            if let Some(disable) = args.get("disable") {
                config.plugins.disable.push(disable.clone());
            }
        }
        "var" => {
            // @var NAME=VALUE
            let args = parse_kv_args(rest)?;
            for (key, value) in &args {
                config.vars.insert(key.clone(), value.clone());
            }
            if args.is_empty() {
                // Bare form: @var NAME=VALUE parsed as a single token
                if let Some(eq) = rest.find('=') {
                    let key = rest[..eq].trim().to_string();
                    let value = rest[eq + 1..].trim().to_string();
                    config.vars.insert(key, value);
                } else {
                    bail!("@var requires NAME=VALUE format");
                }
            }
        }
        "env" => {
            // @env NAME=VALUE — set environment variable in the sandbox process.
            let args = parse_kv_args(rest)?;
            for (key, value) in &args {
                config.env.insert(key.clone(), value.clone());
            }
            if args.is_empty() {
                if let Some(eq) = rest.find('=') {
                    let key = rest[..eq].trim().to_string();
                    let value = rest[eq + 1..].trim().to_string();
                    config.env.insert(key, value);
                } else {
                    bail!("@env requires NAME=VALUE format");
                }
            }
        }
        "env-mode" => {
            // @env-mode inherit|clean
            let mode = rest.trim();
            config.env_mode = Some(match mode {
                "inherit" => super::types::SandboxEnvMode::Inherit,
                "clean" => super::types::SandboxEnvMode::Clean,
                other => bail!("@env-mode must be 'inherit' or 'clean', got '{other}'"),
            });
        }
        "include" => {
            let path = rest.trim().to_string();
            if path.is_empty() {
                bail!("@include requires a file path");
            }
            includes.push((current_step_index, path));
        }
        _ => bail!("unknown config directive: @{name}"),
    }
    Ok(())
}

fn parse_sandbox_config_directive(rest: &str, config: &mut PlaybookConfig) -> Result<()> {
    let path = rest.trim();
    if path.is_empty() {
        bail!("@sandbox-config requires a config file path");
    }
    config.sandbox_config_file = Some(std::path::PathBuf::from(path));
    Ok(())
}

/// Parse a single DSL action line into an [`Action`].
///
/// # Errors
///
/// Returns an error if the action name is unrecognized, required arguments are
/// missing, or argument values fail to parse.
#[allow(clippy::too_many_lines)]
pub fn parse_action_line(line: &str) -> Result<Action> {
    let (action_name, rest) = split_first_token(line);
    let args = parse_kv_args(rest)?;

    match action_name {
        "new-session" => Ok(Action::NewSession {
            name: args.get("name").cloned(),
        }),
        "kill-session" => {
            let name = require_arg(&args, "name", "kill-session")?;
            Ok(Action::KillSession { name })
        }
        "split-pane" => {
            let direction = match args.get("direction").map(String::as_str) {
                Some("vertical" | "v") | None => SplitDirection::Vertical,
                Some("horizontal" | "h") => SplitDirection::Horizontal,
                Some(other) => bail!("invalid split direction: {other}"),
            };
            let ratio = args
                .get("ratio")
                .map(|s| s.parse::<f64>())
                .transpose()
                .context("invalid ratio")?;
            Ok(Action::SplitPane { direction, ratio })
        }
        "focus-pane" => {
            let target: u32 = require_arg(&args, "target", "focus-pane")?
                .parse()
                .context("invalid target index")?;
            Ok(Action::FocusPane { target })
        }
        "close-pane" => {
            let target = args
                .get("target")
                .map(|s| s.parse::<u32>())
                .transpose()
                .context("invalid target index")?;
            Ok(Action::ClosePane { target })
        }
        "send-keys" => {
            let raw = require_arg(&args, "keys", "send-keys")?;
            let bytes = decode_c_escapes(&raw)?;
            let pane = args
                .get("pane")
                .map(|s| s.parse::<u32>())
                .transpose()
                .context("invalid pane index")?;
            Ok(Action::SendKeys { keys: bytes, pane })
        }
        "send-bytes" => {
            let hex_str = require_arg(&args, "hex", "send-bytes")?;
            let bytes = decode_hex(&hex_str)?;
            Ok(Action::SendBytes { hex: bytes })
        }
        "wait-for" => {
            let pattern = require_arg(&args, "pattern", "wait-for")?;
            let pane = args
                .get("pane")
                .map(|s| s.parse::<u32>())
                .transpose()
                .context("invalid pane index")?;
            let timeout_ms: u64 = args
                .get("timeout")
                .map(|s| s.parse())
                .transpose()
                .context("invalid timeout")?
                .unwrap_or(5000);
            let retry: u32 = args
                .get("retry")
                .map(|s| s.parse())
                .transpose()
                .context("invalid retry count")?
                .unwrap_or(1);
            Ok(Action::WaitFor {
                pattern,
                pane,
                timeout: Duration::from_millis(timeout_ms),
                retry,
            })
        }
        "sleep" => {
            let ms: u64 = require_arg(&args, "ms", "sleep")?
                .parse()
                .context("invalid ms")?;
            Ok(Action::Sleep {
                duration: Duration::from_millis(ms),
            })
        }
        "snapshot" => {
            let id = require_arg(&args, "id", "snapshot")?;
            Ok(Action::Snapshot { id })
        }
        "assert-screen" => {
            let pane = args
                .get("pane")
                .map(|s| s.parse::<u32>())
                .transpose()
                .context("invalid pane index")?;
            let contains = args.get("contains").cloned();
            let not_contains = args.get("not_contains").cloned();
            let matches = args.get("matches").cloned();
            let scrollback = args
                .get("scrollback")
                .is_some_and(|value| value == "true" || value == "1");
            if contains.is_none() && not_contains.is_none() && matches.is_none() {
                bail!("assert-screen requires at least one of: contains, not_contains, matches");
            }
            Ok(Action::AssertScreen {
                pane,
                contains,
                not_contains,
                matches,
                scrollback,
            })
        }
        "assert-layout" => {
            let pane_count = args
                .get("pane_count")
                .context("assert-layout requires pane_count")?
                .parse::<u32>()
                .context("invalid pane_count")?;
            Ok(Action::AssertLayout { pane_count })
        }
        "assert-cursor" => {
            let pane = args
                .get("pane")
                .map(|s| s.parse::<u32>())
                .transpose()
                .context("invalid pane index")?;
            let row: u16 = require_arg(&args, "row", "assert-cursor")?
                .parse()
                .context("invalid row")?;
            let col: u16 = require_arg(&args, "col", "assert-cursor")?
                .parse()
                .context("invalid col")?;
            Ok(Action::AssertCursor { pane, row, col })
        }
        "resize-viewport" => {
            let cols: u16 = require_arg(&args, "cols", "resize-viewport")?
                .parse()
                .context("invalid cols")?;
            let rows: u16 = require_arg(&args, "rows", "resize-viewport")?
                .parse()
                .context("invalid rows")?;
            Ok(Action::ResizeViewport { cols, rows })
        }
        "send-attach" => {
            let key = require_arg(&args, "key", "send-attach")?;
            Ok(Action::SendAttach { key })
        }
        "paste-attach" => {
            let text = require_arg(&args, "text", "paste-attach")?;
            Ok(Action::PasteAttach { text })
        }
        "prefix-key" => {
            let key_str = require_arg(&args, "key", "prefix-key")?;
            let key = key_str.chars().next().context("empty key")?;
            Ok(Action::PrefixKey { key })
        }
        "wait-for-event" => {
            let event = require_arg(&args, "event", "wait-for-event")?;
            let timeout_ms: u64 = args
                .get("timeout")
                .map(|s| s.parse())
                .transpose()
                .context("invalid timeout")?
                .unwrap_or(5000);
            Ok(Action::WaitForEvent {
                event,
                timeout: Duration::from_millis(timeout_ms),
            })
        }
        "invoke-service" => {
            let capability = require_arg(&args, "capability", "invoke-service")?;
            let kind = match args.get("kind").map(String::as_str) {
                Some("query" | "q") => ServiceKind::Query,
                Some("command" | "cmd") | None => ServiceKind::Command,
                Some(other) => {
                    bail!("invalid service kind: {other} (expected 'query' or 'command')")
                }
            };
            let interface_id = require_arg(&args, "interface", "invoke-service")?;
            let operation = require_arg(&args, "operation", "invoke-service")?;
            let payload = args.get("payload").cloned().unwrap_or_default();
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
            let id = require_arg(&args, "id", "render-mark")?;
            Ok(Action::RenderMark { id })
        }
        "assert-render" => {
            let since = require_arg(&args, "since", "assert-render")?;
            Ok(Action::AssertRender {
                since,
                assertion: parse_render_assertion(&args)?,
            })
        }
        "seed-window-list" => {
            let names = require_arg(&args, "names", "seed-window-list")?
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            let active = require_arg(&args, "active", "seed-window-list")?;
            Ok(Action::SeedWindowList { names, active })
        }
        "seed-pane-text" => {
            let lines = require_arg(&args, "lines", "seed-pane-text")?
                .split('|')
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            let cursor_row = require_arg(&args, "cursor_row", "seed-pane-text")?.parse()?;
            let cursor_col = require_arg(&args, "cursor_col", "seed-pane-text")?.parse()?;
            Ok(Action::SeedPaneText {
                lines,
                cursor_row,
                cursor_col,
            })
        }
        "seed-pane-layout" => Ok(Action::SeedPaneLayout {
            split: require_arg(&args, "split", "seed-pane-layout")?,
        }),
        "render" => Ok(Action::Render),
        "locate" => Ok(Action::Locate {
            id: require_arg(&args, "id", "locate")?,
            text: require_arg(&args, "text", "locate")?,
        }),
        "terminal-event" => Ok(Action::TerminalEvent(SimTerminalEvent {
            kind: require_arg(&args, "kind", "terminal-event")?,
            phase: require_arg(&args, "phase", "terminal-event")?,
            button: args.get("button").cloned(),
            col: require_arg(&args, "col", "terminal-event")?,
            row: require_arg(&args, "row", "terminal-event")?,
        })),
        "assert-effect" => Ok(Action::AssertEffect {
            operation: require_arg(&args, "operation", "assert-effect")?,
        }),
        "assert-no-effect" => Ok(Action::AssertNoEffect {
            operation: require_arg(&args, "operation", "assert-no-effect")?,
        }),
        "assert-state" => Ok(Action::AssertState {
            path: require_arg(&args, "path", "assert-state")?,
            equals: require_arg(&args, "equals", "assert-state")?,
        }),
        "assert-rendered" => Ok(Action::AssertRendered {
            contains: args.get("contains").cloned(),
            matches: args.get("matches").cloned(),
        }),
        "set-config" => Ok(Action::SetConfig {
            path: require_arg(&args, "path", "set-config")?,
            value: require_arg(&args, "value", "set-config")?,
        }),
        _ => bail!("unknown action: {action_name}"),
    }
}

fn parse_render_assertion(args: &BTreeMap<String, String>) -> Result<RenderAssertion> {
    let mut assertion = RenderAssertion::default();
    macro_rules! parse_u64_opt {
        ($field:ident) => {
            assertion.$field = args
                .get(stringify!($field))
                .map(|value| value.parse::<u64>())
                .transpose()
                .with_context(|| format!("invalid {}", stringify!($field)))?;
        };
    }
    macro_rules! parse_bool_opt {
        ($field:ident) => {
            assertion.$field = args
                .get(stringify!($field))
                .map(|value| value.parse::<bool>())
                .transpose()
                .with_context(|| format!("invalid {}", stringify!($field)))?;
        };
    }
    parse_u64_opt!(min_frames);
    parse_u64_opt!(max_frames);
    parse_bool_opt!(full_frame);
    parse_u64_opt!(max_full_frame_frames);
    parse_u64_opt!(max_full_surface_fallbacks);
    parse_u64_opt!(max_damage_rects);
    parse_u64_opt!(max_damage_area_cells);
    parse_u64_opt!(max_rows_emitted);
    parse_u64_opt!(max_row_segments_emitted);
    parse_u64_opt!(max_cells_emitted);
    parse_u64_opt!(max_frame_bytes);
    parse_bool_opt!(status_rendered);
    parse_bool_opt!(overlay_rendered);
    assertion.expected_emitted_rows = args
        .get("expected_emitted_rows")
        .map(|value| parse_render_row_refs(value))
        .transpose()?;
    assertion.expected_emitted_row_segments = args
        .get("expected_emitted_row_segments")
        .map(|value| parse_render_row_segment_refs(value))
        .transpose()?;
    assertion.expected_trace_ops = args
        .get("expected_trace_ops")
        .map(|value| parse_render_trace_ops(value))
        .transpose()?;
    Ok(assertion)
}

fn parse_render_row_refs(value: &str) -> Result<Vec<PlaybookRenderRowRef>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    trimmed
        .split(',')
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

fn parse_render_row_segment_refs(value: &str) -> Result<Vec<PlaybookRenderRowSegmentRef>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    trimmed
        .split(',')
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

fn parse_render_trace_ops(value: &str) -> Result<Vec<PlaybookRenderTraceOp>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    trimmed
        .split(',')
        .map(|entry| PlaybookRenderTraceOp::parse_compact(entry).map_err(anyhow::Error::msg))
        .collect()
}

fn require_arg(args: &BTreeMap<String, String>, key: &str, action: &str) -> Result<String> {
    args.get(key)
        .cloned()
        .with_context(|| format!("{action} requires '{key}' argument"))
}

/// Split on the first whitespace to get the action name and remaining text.
fn split_first_token(s: &str) -> (&str, &str) {
    s.find(char::is_whitespace)
        .map_or((s, ""), |pos| (&s[..pos], s[pos..].trim_start()))
}

/// Parse `key=value key2='val with spaces'` argument pairs.
fn parse_kv_args(input: &str) -> Result<BTreeMap<String, String>> {
    let mut args = BTreeMap::new();
    let mut remaining = input.trim();

    while !remaining.is_empty() {
        // Find the '=' separator
        let eq_pos = remaining
            .find('=')
            .with_context(|| format!("expected key=value, got: {remaining}"))?;
        let key = remaining[..eq_pos].trim().to_string();
        remaining = remaining[eq_pos + 1..].trim_start();

        // Parse the value (possibly quoted)
        let (value, rest) = parse_value(remaining)?;
        args.insert(key, value);
        remaining = rest.trim_start();
    }

    Ok(args)
}

/// Parse a single value — quoted or bare (up to next whitespace).
fn parse_value(input: &str) -> Result<(String, &str)> {
    if input.is_empty() {
        return Ok((String::new(), ""));
    }

    let first = input.as_bytes()[0];
    if first == b'\'' || first == b'"' {
        parse_quoted_value(input, first as char)
    } else {
        // Bare value: up to next whitespace
        input.find(char::is_whitespace).map_or_else(
            || Ok((input.to_string(), "")),
            |pos| Ok((input[..pos].to_string(), &input[pos..])),
        )
    }
}

/// Parse a quoted value, consuming the opening and closing quote.
fn parse_quoted_value(input: &str, quote: char) -> Result<(String, &str)> {
    let bytes = input.as_bytes();
    debug_assert_eq!(bytes[0], quote as u8);
    let quote_byte = quote as u8;
    let mut result = Vec::new();
    let mut i = 1; // skip opening quote

    while i < bytes.len() {
        if bytes[i] == quote_byte {
            return Ok((String::from_utf8(result)?, &input[i + 1..]));
        }
        if bytes[i] == b'\\' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'\'' | b'"') {
            result.push(bytes[i + 1]);
            i += 2;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }

    bail!("unterminated quoted string");
}

/// Decode C-style escape sequences in a string value into raw bytes.
/// # Errors
///
/// Returns an error if the input contains invalid escape sequences.
pub fn decode_c_escapes(input: &str) -> Result<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'n' => result.push(b'\n'),
                b'r' => result.push(b'\r'),
                b't' => result.push(b'\t'),
                b'\\' => result.push(b'\\'),
                b'\'' => result.push(b'\''),
                b'"' => result.push(b'"'),
                b'0' => result.push(0),
                b'a' => result.push(0x07),
                b'b' => result.push(0x08),
                b'e' => result.push(0x1b),
                b'x' if i + 2 < bytes.len() => {
                    let hex = &input[i + 1..i + 3];
                    let byte = u8::from_str_radix(hex, 16)
                        .with_context(|| format!("invalid hex escape: \\x{hex}"))?;
                    result.push(byte);
                    i += 2;
                }
                other => {
                    // Unknown escape: keep literal
                    result.push(b'\\');
                    result.push(other);
                }
            }
        } else {
            result.push(bytes[i]);
        }
        i += 1;
    }

    Ok(result)
}

/// Decode hex string to bytes (e.g. "1b5b41" -> [0x1b, 0x5b, 0x41]).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_playbook() {
        let input = r"
# A simple test playbook
@viewport cols=120 rows=50
@timeout 10000
@name test-playbook

new-session name=main
send-keys keys='echo hello\r'
wait-for pattern='hello' timeout=3000
sleep ms=100
snapshot id=final
";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        assert_eq!(playbook.config.name.as_deref(), Some("test-playbook"));
        assert_eq!(playbook.config.viewport.cols, 120);
        assert_eq!(playbook.config.viewport.rows, 50);
        assert_eq!(playbook.config.timeout, Duration::from_secs(10));
        assert_eq!(playbook.steps.len(), 5);
        assert_eq!(playbook.steps[0].action.name(), "new-session");
        assert_eq!(playbook.steps[1].action.name(), "send-keys");
        assert_eq!(playbook.steps[2].action.name(), "wait-for");
        assert_eq!(playbook.steps[3].action.name(), "sleep");
        assert_eq!(playbook.steps[4].action.name(), "snapshot");
    }

    #[test]
    fn parse_send_keys_escapes() {
        let input = "send-keys keys='hello\\r\\n'";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::SendKeys { keys, .. } => {
                assert_eq!(keys, b"hello\r\n");
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn parse_send_keys_preserves_double_escaped_shell_sequences() {
        let input = r#"send-keys keys='printf \"\\e[2J\\e[H\\n\"\r'"#;
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::SendKeys { keys, .. } => {
                assert_eq!(keys, b"printf \"\\e[2J\\e[H\\n\"\r");
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn parse_send_keys_hex_escape() {
        let input = "send-keys keys='\\x1b[A'";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::SendKeys { keys, .. } => {
                assert_eq!(keys, b"\x1b[A");
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn parse_send_bytes_hex() {
        let input = "send-bytes hex=1b5b41";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::SendBytes { hex } => {
                assert_eq!(hex, &[0x1b, 0x5b, 0x41]);
            }
            _ => panic!("expected send-bytes"),
        }
    }

    #[test]
    fn parse_split_pane_defaults() {
        let input = "split-pane";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::SplitPane { direction, ratio } => {
                assert_eq!(*direction, SplitDirection::Vertical);
                assert!(ratio.is_none());
            }
            _ => panic!("expected split-pane"),
        }
    }

    #[test]
    fn parse_plugin_config() {
        let input = "@plugin enable=bmux.windows\n@plugin disable=bmux.permissions";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        assert_eq!(playbook.config.plugins.enable, vec!["bmux.windows"]);
        assert_eq!(playbook.config.plugins.disable, vec!["bmux.permissions"]);
    }

    #[test]
    fn parse_assert_screen() {
        let input = "assert-screen pane=0 contains='hello world'";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::AssertScreen {
                pane,
                contains,
                not_contains,
                matches,
                scrollback,
            } => {
                assert_eq!(*pane, Some(0));
                assert_eq!(contains.as_deref(), Some("hello world"));
                assert!(not_contains.is_none());
                assert!(matches.is_none());
                assert!(!*scrollback);
            }
            _ => panic!("expected assert-screen"),
        }
    }

    #[test]
    fn decode_c_escapes_ctrl_a() {
        let bytes = decode_c_escapes("\\x01").unwrap();
        assert_eq!(bytes, vec![0x01]);
    }

    #[test]
    fn decode_c_escapes_mixed() {
        let bytes = decode_c_escapes("hello\\r\\nworld").unwrap();
        assert_eq!(bytes, b"hello\r\nworld");
    }

    #[test]
    fn empty_input_produces_empty_playbook() {
        let (playbook, _) = parse_dsl("").unwrap();
        assert!(playbook.steps.is_empty());
    }

    #[test]
    fn comments_and_blanks_ignored() {
        let input = "\n# comment\n   \n# another comment\n";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        assert!(playbook.steps.is_empty());
    }

    #[test]
    fn unknown_action_fails() {
        let result = parse_dsl("nonexistent-action foo=bar");
        assert!(result.is_err());
    }

    #[test]
    fn unknown_directive_fails() {
        let result = parse_dsl("@bogus-directive something");
        assert!(result.is_err());
    }

    #[test]
    fn parse_resize_viewport() {
        let input = "resize-viewport cols=132 rows=50";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::ResizeViewport { cols, rows } => {
                assert_eq!(*cols, 132);
                assert_eq!(*rows, 50);
            }
            _ => panic!("expected resize-viewport"),
        }
    }

    #[test]
    fn parse_prefix_key() {
        let input = "prefix-key key=c";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::PrefixKey { key } => {
                assert_eq!(*key, 'c');
            }
            _ => panic!("expected prefix-key"),
        }
    }

    #[test]
    fn parse_send_attach() {
        let input = "send-attach key='ctrl+a ['";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::SendAttach { key } => assert_eq!(key, "ctrl+a ["),
            _ => panic!("expected send-attach"),
        }
    }

    #[test]
    fn parse_sandbox_config() {
        let input = "@sandbox-config /tmp/bmux.toml\nnew-session\n";
        let (playbook, _includes) = parse_dsl(input).unwrap();
        assert_eq!(
            playbook.config.sandbox_config_file.as_deref(),
            Some(std::path::Path::new("/tmp/bmux.toml"))
        );
    }

    #[test]
    fn double_quoted_value() {
        let input = r#"send-keys keys="echo hello\r""#;
        let (playbook, _includes) = parse_dsl(input).unwrap();
        match &playbook.steps[0].action {
            Action::SendKeys { keys, .. } => {
                assert_eq!(keys, b"echo hello\r");
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn parse_env_mode_clean() {
        let input = "@env-mode clean\nnew-session\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        assert_eq!(
            playbook.config.env_mode,
            Some(super::super::types::SandboxEnvMode::Clean)
        );
    }

    #[test]
    fn parse_env_mode_inherit() {
        let input = "@env-mode inherit\nnew-session\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        assert_eq!(
            playbook.config.env_mode,
            Some(super::super::types::SandboxEnvMode::Inherit)
        );
    }

    #[test]
    fn parse_env_mode_invalid_fails() {
        let input = "@env-mode foobar\nnew-session\n";
        assert!(parse_dsl(input).is_err());
    }

    #[test]
    fn parse_env_directive() {
        let input = "@env FOO=bar\n@env BAZ=qux\nnew-session\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        assert_eq!(playbook.config.env.get("FOO").unwrap(), "bar");
        assert_eq!(playbook.config.env.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn parse_screen_action() {
        let input = "new-session\nscreen\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        assert!(matches!(playbook.steps[1].action, Action::Screen));
    }

    #[test]
    fn parse_status_action() {
        let input = "new-session\nstatus\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        assert!(matches!(playbook.steps[1].action, Action::Status));
    }

    #[test]
    fn parse_wait_for_retry() {
        let input = "new-session\nwait-for pattern='hello' retry=3\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        match &playbook.steps[1].action {
            Action::WaitFor { retry, .. } => assert_eq!(*retry, 3),
            other => panic!("expected WaitFor, got {other:?}"),
        }
    }

    #[test]
    fn parse_continue_on_error_suffix() {
        let input = "new-session\nassert-screen contains='hello' !continue\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        assert!(playbook.steps[1].continue_on_error);
    }

    #[test]
    fn parse_no_continue_on_error_by_default() {
        let input = "new-session\nassert-screen contains='hello'\n";
        let (playbook, _) = parse_dsl(input).unwrap();
        assert!(!playbook.steps[1].continue_on_error);
    }
}
