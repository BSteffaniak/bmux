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

/// Convert a list of recording events into a DSL playbook string.
pub fn events_to_playbook(events: &[RecordingEventEnvelope]) -> Result<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push("# Auto-generated from recording".to_string());
    lines.push(String::new());

    let mut last_mono_ns: u64 = 0;
    let mut has_session = false;

    // The recording often starts after the session was already created (since
    // the playbook engine starts recording after new-session succeeds). If no
    // NewSession event is found, synthesize one at the beginning.
    let has_new_session_event = events.iter().any(|e| {
        if let (
            RecordingEventKind::RequestStart,
            RecordingPayload::RequestStart { request_data, .. },
        ) = (&e.kind, &e.payload)
        {
            if let Ok(Request::NewSession { .. }) = bmux_ipc::decode::<Request>(request_data) {
                return true;
            }
        }
        false
    });
    if !has_new_session_event {
        lines.push("new-session".to_string());
        lines.push("sleep ms=500".to_string());
        has_session = true;
    }

    for event in events {
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
                RecordingPayload::RequestStart { request_data, .. },
            ) => {
                if request_data.is_empty() {
                    continue;
                }
                if let Ok(request) = bmux_ipc::decode::<Request>(request_data) {
                    if let Some(line) = request_to_dsl(&request, &mut has_session) {
                        lines.push(line);
                    }
                }
            }
            _ => {}
        }
    }

    lines.push(String::new());
    lines.push("# TODO: add assertions".to_string());

    Ok(lines.join("\n"))
}

/// Convert a `Request` variant to a DSL action line, if applicable.
fn request_to_dsl(request: &Request, has_session: &mut bool) -> Option<String> {
    match request {
        Request::NewSession { name } => {
            *has_session = true;
            match name {
                Some(n) => Some(format!("new-session name='{n}'")),
                None => Some("new-session".to_string()),
            }
        }
        Request::SplitPane { direction, .. } => {
            let dir = match direction {
                PaneSplitDirection::Vertical => "vertical",
                PaneSplitDirection::Horizontal => "horizontal",
            };
            Some(format!("split-pane direction={dir}"))
        }
        Request::FocusPane { target, .. } => {
            // target is Option<PaneSelector>, extract index if available
            match target {
                Some(bmux_ipc::PaneSelector::ByIndex(idx)) => {
                    Some(format!("focus-pane target={idx}"))
                }
                _ => Some("# focus-pane (direction-based, manual edit needed)".to_string()),
            }
        }
        Request::ClosePane { .. } => Some("close-pane".to_string()),
        Request::KillSession { selector, .. } => match selector {
            bmux_ipc::SessionSelector::ByName(name) => Some(format!("kill-session name='{name}'")),
            _ => Some("# kill-session (by id, manual edit needed)".to_string()),
        },
        Request::AttachSetViewport { cols, rows, .. } => {
            Some(format!("resize-viewport cols={cols} rows={rows}"))
        }
        Request::AttachInput { data, .. } => {
            if data.is_empty() || !*has_session {
                return None;
            }
            let escaped = bytes_to_c_escaped(data);
            Some(format!("send-keys keys='{escaped}'"))
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
        | Request::PollEvents { .. } => None,
        // Recording-related requests aren't playbook actions.
        Request::RecordingStart { .. }
        | Request::RecordingStop { .. }
        | Request::RecordingStatus
        | Request::RecordingList
        | Request::RecordingDelete { .. }
        | Request::RecordingDeleteAll
        | Request::RecordingWriteCustomEvent { .. } => None,
        // Attach lifecycle is handled implicitly by the playbook engine.
        Request::Attach { .. }
        | Request::AttachContext { .. }
        | Request::AttachOpen { .. }
        | Request::Detach => None,
        // Everything else gets a comment for manual review.
        _ => Some(format!(
            "# unhandled request: {:?}",
            std::mem::discriminant(request)
        )),
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
