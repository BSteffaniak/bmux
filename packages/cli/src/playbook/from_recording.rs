//! Convert a recording into a playbook stub.
//!
//! Reads the recording's `events.bin`, extracts structural requests and input
//! events, and outputs a line-oriented DSL playbook.

use anyhow::Result;
use bmux_ipc::{
    PaneSplitDirection, RecordingEventEnvelope, RecordingEventKind, RecordingPayload, Request,
};

/// Minimum timing gap (in nanoseconds) before inserting a `sleep` step.
const SLEEP_THRESHOLD_NS: u64 = 500_000_000; // 500ms

/// Maximum timing gap (in nanoseconds) between consecutive `AttachInput` events
/// to coalesce them into a single `send-keys` line.
const INPUT_COALESCE_NS: u64 = 100_000_000; // 100ms

/// Convert a list of recording events into a DSL playbook string.
pub fn events_to_playbook(events: &[RecordingEventEnvelope]) -> Result<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push("# Auto-generated from recording".to_string());
    lines.push(String::new());

    let mut last_mono_ns: u64 = 0;
    let mut has_session = false;
    // Accumulator for coalescing consecutive AttachInput events.
    let mut pending_input: Vec<u8> = Vec::new();
    let mut last_input_mono_ns: u64 = 0;

    for event in events {
        // Check if we need to flush pending input before a timing gap or non-input event.
        let is_attach_input = matches!(
            (&event.kind, &event.payload),
            (RecordingEventKind::RequestStart, RecordingPayload::RequestStart { request_kind, .. })
            if request_kind == "attach_input"
        );

        let time_gap = if last_input_mono_ns > 0 && event.mono_ns > last_input_mono_ns {
            event.mono_ns - last_input_mono_ns
        } else {
            0
        };

        // Flush pending input if: this isn't an input event, or the timing gap exceeds the
        // coalescing threshold.
        if !pending_input.is_empty() && (!is_attach_input || time_gap > INPUT_COALESCE_NS) {
            let escaped = bytes_to_c_escaped(&pending_input);
            lines.push(format!("send-keys keys='{escaped}'"));
            pending_input.clear();
        }

        // Insert sleep for timing gaps.
        if last_mono_ns > 0 && event.mono_ns > last_mono_ns {
            let gap_ns = event.mono_ns - last_mono_ns;
            if gap_ns >= SLEEP_THRESHOLD_NS {
                let gap_ms = gap_ns / 1_000_000;
                lines.push(format!("sleep ms={gap_ms}"));
            }
        }
        last_mono_ns = event.mono_ns;

        match (&event.kind, &event.payload) {
            // Structural requests — decode the full Request from postcard bytes.
            (
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart {
                    request_data,
                    request_kind,
                    ..
                },
            ) => {
                if request_data.is_empty() {
                    continue;
                }
                if let Ok(request) = bmux_ipc::decode::<Request>(request_data) {
                    match request_to_dsl(&request, &mut has_session, request_kind) {
                        RequestDslResult::Line(line) => lines.push(line),
                        RequestDslResult::CoalesceInput(data) => {
                            pending_input.extend_from_slice(&data);
                            last_input_mono_ns = event.mono_ns;
                        }
                        RequestDslResult::Skip => {}
                    }
                }
            }
            _ => {}
        }
    }

    // Flush any remaining pending input.
    if !pending_input.is_empty() {
        let escaped = bytes_to_c_escaped(&pending_input);
        lines.push(format!("send-keys keys='{escaped}'"));
    }

    lines.push(String::new());
    lines.push("# TODO: add assertions".to_string());

    Ok(lines.join("\n"))
}

/// Result of converting a Request to a DSL line.
enum RequestDslResult {
    /// A complete DSL line to emit.
    Line(String),
    /// Input bytes to coalesce with subsequent AttachInput events.
    CoalesceInput(Vec<u8>),
    /// Skip this request (non-structural).
    Skip,
}

/// Convert a `Request` variant to a DSL action line, if applicable.
fn request_to_dsl(
    request: &Request,
    has_session: &mut bool,
    request_kind: &str,
) -> RequestDslResult {
    match request {
        Request::NewSession { name } => {
            *has_session = true;
            RequestDslResult::Line(match name {
                Some(n) => format!("new-session name='{n}'"),
                None => "new-session".to_string(),
            })
        }
        Request::SplitPane { direction, .. } => {
            let dir = match direction {
                PaneSplitDirection::Vertical => "vertical",
                PaneSplitDirection::Horizontal => "horizontal",
            };
            RequestDslResult::Line(format!("split-pane direction={dir}"))
        }
        Request::FocusPane { target, .. } => match target {
            Some(bmux_ipc::PaneSelector::ByIndex(idx)) => {
                RequestDslResult::Line(format!("focus-pane target={idx}"))
            }
            _ => RequestDslResult::Line(
                "# focus-pane (direction-based, manual edit needed)".to_string(),
            ),
        },
        Request::ClosePane { .. } => RequestDslResult::Line("close-pane".to_string()),
        Request::KillSession { selector, .. } => match selector {
            bmux_ipc::SessionSelector::ByName(name) => {
                RequestDslResult::Line(format!("kill-session name='{name}'"))
            }
            _ => RequestDslResult::Line("# kill-session (by id, manual edit needed)".to_string()),
        },
        Request::AttachSetViewport { cols, rows, .. } => {
            RequestDslResult::Line(format!("resize-viewport cols={cols} rows={rows}"))
        }
        Request::ResizePane { delta, .. } => {
            RequestDslResult::Line(format!("# resize-pane delta={delta}"))
        }
        Request::AttachInput { data, .. } => {
            if data.is_empty() || !*has_session {
                return RequestDslResult::Skip;
            }
            RequestDslResult::CoalesceInput(data.clone())
        }
        // Skip high-frequency / non-structural requests.
        Request::AttachOutput { .. }
        | Request::AttachSnapshot { .. }
        | Request::AttachLayout { .. }
        | Request::AttachPaneOutputBatch { .. }
        | Request::Ping
        | Request::Hello { .. }
        | Request::WhoAmI
        | Request::WhoAmIPrincipal
        | Request::ListSessions
        | Request::ListPanes { .. }
        | Request::ListClients
        | Request::SubscribeEvents
        | Request::PollEvents { .. } => RequestDslResult::Skip,
        // Recording-related requests aren't playbook actions.
        Request::RecordingStart { .. }
        | Request::RecordingStop { .. }
        | Request::RecordingStatus
        | Request::RecordingList
        | Request::RecordingDelete { .. }
        | Request::RecordingDeleteAll
        | Request::RecordingWriteCustomEvent { .. } => RequestDslResult::Skip,
        // Attach lifecycle is handled implicitly by the playbook engine.
        Request::Attach { .. }
        | Request::AttachContext { .. }
        | Request::AttachOpen { .. }
        | Request::Detach => RequestDslResult::Skip,
        // Everything else gets a comment for manual review.
        _ => RequestDslResult::Line(format!("# unhandled request: {request_kind}")),
    }
}

/// Escape bytes to C-style escape string for use in `send-keys keys='...'`.
fn bytes_to_c_escaped(data: &[u8]) -> String {
    let mut result = String::new();
    for &byte in data {
        match byte {
            b'\r' => result.push_str("\\r"),
            b'\n' => result.push_str("\\n"),
            b'\t' => result.push_str("\\t"),
            b'\\' => result.push_str("\\\\"),
            b'\'' => result.push_str("\\'"),
            0x1b => result.push_str("\\e"),
            0x01..=0x1a => {
                // Ctrl+A through Ctrl+Z
                result.push_str(&format!("\\x{byte:02x}"));
            }
            0x7f => result.push_str("\\x7f"),
            0x20..=0x7e => result.push(byte as char),
            _ => result.push_str(&format!("\\x{byte:02x}")),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_to_c_escaped_basic() {
        assert_eq!(bytes_to_c_escaped(b"hello\r\n"), "hello\\r\\n");
    }

    #[test]
    fn bytes_to_c_escaped_ctrl() {
        assert_eq!(bytes_to_c_escaped(&[0x01]), "\\x01"); // Ctrl+A
        assert_eq!(bytes_to_c_escaped(&[0x1b]), "\\e"); // Escape
    }

    #[test]
    fn bytes_to_c_escaped_mixed() {
        assert_eq!(bytes_to_c_escaped(b"echo hello\r"), "echo hello\\r");
    }
}
