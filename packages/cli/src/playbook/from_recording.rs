//! Convert a recording into a playbook with assertions.
//!
//! Reads the recording's `events.bin`, extracts structural requests, input
//! events, and output events, then produces a line-oriented DSL playbook with
//! `wait-for` barriers and `assert-screen` checks generated from the recorded
//! terminal output.
//!
//! ## Approach
//!
//! 1. **State tracking** — a `RecordingStateTracker` processes the event stream
//!    to track pane creation/destruction, focus changes, and viewport dimensions.
//!    This lets us attribute input events to specific panes and use correct
//!    terminal dimensions for vt100 parsing.
//!
//! 2. **Input/output correlation** — input events are grouped with subsequent
//!    output events from the same pane (the "response window") until the next
//!    input or a quiescent gap.
//!
//! 3. **Assertion generation** — output in each response window is parsed through
//!    a vt100 terminal emulator to extract the rendered screen. The last non-empty
//!    line (typically a shell prompt after command completion) becomes a `wait-for`
//!    barrier. Distinctive content lines become `assert-screen contains=` checks.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use bmux_ipc::{
    PaneSplitDirection, RecordingEventEnvelope, RecordingEventKind, RecordingPayload, Request,
    ResponsePayload,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Timing thresholds
// ---------------------------------------------------------------------------

/// Minimum gap (ns) before inserting a `sleep` step.
const SLEEP_THRESHOLD_NS: u64 = 200_000_000; // 200ms (lowered from 500ms for fidelity)

/// Maximum gap (ns) between consecutive inputs before flushing an input batch.
const INPUT_COALESCE_NS: u64 = 100_000_000; // 100ms

/// Quiescent gap (ns) after the last output event before considering the
/// response window closed and generating assertions.
const OUTPUT_QUIESCENT_NS: u64 = 300_000_000; // 300ms

// ---------------------------------------------------------------------------
// Pane state tracker
// ---------------------------------------------------------------------------

/// Tracks pane lifecycle and focus state from the recording event stream.
struct RecordingStateTracker {
    /// Map from pane UUID to its stable index in the layout order.
    pane_uuid_to_index: BTreeMap<Uuid, u32>,
    /// The UUID of the currently focused pane (if known).
    focused_pane_id: Option<Uuid>,
    /// Terminal viewport dimensions (cols, rows).
    viewport: (u16, u16),
    /// Number of panes created so far (used for index assignment).
    next_pane_index: u32,
}

impl RecordingStateTracker {
    const fn new() -> Self {
        Self {
            pane_uuid_to_index: BTreeMap::new(),
            focused_pane_id: None,
            viewport: (80, 24),
            next_pane_index: 0,
        }
    }

    /// Register a new pane with a UUID, assigning the next sequential index.
    fn add_pane(&mut self, pane_id: Uuid) {
        if !self.pane_uuid_to_index.contains_key(&pane_id) {
            self.pane_uuid_to_index
                .insert(pane_id, self.next_pane_index);
            self.next_pane_index += 1;
        }
    }

    /// Remove a pane (on close).
    fn remove_pane(&mut self, pane_id: &Uuid) {
        self.pane_uuid_to_index.remove(pane_id);
    }

    /// Set the focused pane.
    const fn set_focus(&mut self, pane_id: Uuid) {
        self.focused_pane_id = Some(pane_id);
    }

    /// Get the index for a pane UUID, if known.
    fn pane_index(&self, pane_id: &Uuid) -> Option<u32> {
        self.pane_uuid_to_index.get(pane_id).copied()
    }

    /// Get the index of the focused pane.
    fn focused_pane_index(&self) -> Option<u32> {
        self.focused_pane_id
            .as_ref()
            .and_then(|id| self.pane_index(id))
    }

    /// Update state from a decoded Request.
    const fn process_request(&mut self, request: &Request) {
        if let Request::AttachSetViewport { cols, rows, .. } = request {
            self.viewport = (*cols, *rows);
        }
    }

    /// Update state from a decoded `ResponsePayload`.
    fn process_response(&mut self, response: &ResponsePayload) {
        match response {
            ResponsePayload::PaneSplit { id, .. } => {
                self.add_pane(*id);
            }
            ResponsePayload::PaneFocused { id, .. } => {
                self.set_focus(*id);
            }
            ResponsePayload::PaneClosed { id, .. } => {
                self.remove_pane(id);
            }
            ResponsePayload::AttachSnapshot {
                focused_pane_id,
                panes,
                ..
            } => {
                // Snapshot responses give us authoritative pane state.
                for pane in panes {
                    self.add_pane(pane.id);
                }
                self.set_focus(*focused_pane_id);
            }
            ResponsePayload::AttachLayout {
                focused_pane_id,
                panes,
                ..
            } => {
                for pane in panes {
                    self.add_pane(pane.id);
                }
                self.set_focus(*focused_pane_id);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Output accumulator
// ---------------------------------------------------------------------------

/// Accumulated output bytes for a specific pane during a response window.
struct PaneOutputAccumulator {
    pane_id: Uuid,
    bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert a list of recording events into a DSL playbook string with assertions.
///
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn events_to_playbook(events: &[RecordingEventEnvelope]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("# Auto-generated from recording".to_string());
    lines.push(String::new());

    let mut state = RecordingStateTracker::new();
    let mut last_mono_ns: u64 = 0;
    let mut has_session = false;

    // Accumulator for coalescing consecutive AttachInput events.
    let mut pending_input: Vec<u8> = Vec::new();
    let mut pending_input_pane: Option<Uuid> = None;
    let mut last_input_mono_ns: u64 = 0;
    // Tracks whether the pending input ends with \r (command execution).
    let mut pending_input_is_command = false;

    // Output accumulator: collects PaneOutputRaw bytes between input events.
    let mut output_accum: Vec<PaneOutputAccumulator> = Vec::new();
    let mut last_output_mono_ns: u64 = 0;
    let mut viewport_set = false;

    for event in events {
        // ---- Update state from RequestDone events ----
        if let (
            RecordingEventKind::RequestDone,
            RecordingPayload::RequestDone {
                request_data,
                response_data,
                ..
            },
        ) = (&event.kind, &event.payload)
        {
            if !request_data.is_empty()
                && let Ok(request) = bmux_ipc::decode::<Request>(request_data)
            {
                state.process_request(&request);
            }
            if !response_data.is_empty()
                && let Ok(response) = bmux_ipc::decode::<ResponsePayload>(response_data)
            {
                state.process_response(&response);
            }
        }

        // ---- Detect input events ----
        let is_input_event = matches!(
            (&event.kind, &event.payload),
            (
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart { request_kind, .. }
            ) if request_kind == "attach_input" || request_kind == "pane_direct_input"
        );

        let time_gap = if last_input_mono_ns > 0 && event.mono_ns > last_input_mono_ns {
            event.mono_ns - last_input_mono_ns
        } else {
            0
        };

        // Check if output has been quiescent (indicating a response window is complete).
        let output_quiescent = !output_accum.is_empty()
            && last_output_mono_ns > 0
            && event.mono_ns.saturating_sub(last_output_mono_ns) > OUTPUT_QUIESCENT_NS;

        // ---- Flush pending input + generate assertions from output ----
        let should_flush_input =
            !pending_input.is_empty() && (!is_input_event || time_gap > INPUT_COALESCE_NS);

        if should_flush_input || (output_quiescent && pending_input.is_empty()) {
            if !pending_input.is_empty() {
                flush_input(&mut lines, &pending_input, pending_input_pane, &state);
                let was_command = pending_input_is_command;
                pending_input.clear();
                pending_input_pane = None;
                pending_input_is_command = false;

                // Generate assertions from output accumulated since the last input.
                if was_command && !output_accum.is_empty() {
                    generate_assertions_from_output(&mut lines, &output_accum, &state);
                }
                output_accum.clear();
            } else if output_quiescent {
                // Output quiescent with no pending input — generate assertions
                // for startup output or other non-input-driven output.
                generate_assertions_from_output(&mut lines, &output_accum, &state);
                output_accum.clear();
            }
        }

        // ---- Insert sleep for timing gaps ----
        if last_mono_ns > 0 && event.mono_ns > last_mono_ns {
            let gap_ns = event.mono_ns - last_mono_ns;
            if gap_ns >= SLEEP_THRESHOLD_NS {
                let gap_ms = gap_ns / 1_000_000;
                lines.push(format!("sleep ms={gap_ms}"));
            }
        }
        last_mono_ns = event.mono_ns;

        // ---- Accumulate output ----
        if let (RecordingEventKind::PaneOutputRaw, RecordingPayload::Bytes { data }) =
            (&event.kind, &event.payload)
        {
            let pane_id = event.pane_id.unwrap_or_default();
            if let Some(existing) = output_accum.iter_mut().find(|a| a.pane_id == pane_id) {
                existing.bytes.extend_from_slice(data);
            } else {
                output_accum.push(PaneOutputAccumulator {
                    pane_id,
                    bytes: data.clone(),
                });
            }
            last_output_mono_ns = event.mono_ns;
        }

        // ---- Process structural requests ----
        if let (
            RecordingEventKind::RequestStart,
            RecordingPayload::RequestStart {
                request_data,
                request_kind,
                ..
            },
        ) = (&event.kind, &event.payload)
        {
            if request_data.is_empty() {
                continue;
            }
            if let Ok(request) = bmux_ipc::decode::<Request>(request_data) {
                // Emit viewport directive on first AttachSetViewport.
                if let Request::AttachSetViewport { cols, rows, .. } = &request
                    && !viewport_set
                {
                    // Insert viewport as the first directive after the header.
                    let insert_pos = lines
                        .iter()
                        .position(std::string::String::is_empty)
                        .unwrap_or(1)
                        + 1;
                    lines.insert(insert_pos, format!("@viewport cols={cols} rows={rows}"));
                    viewport_set = true;
                }

                match request_to_dsl(&request, &mut has_session, request_kind, &state, event) {
                    RequestDslResult::Line(line) => lines.push(line),
                    RequestDslResult::CoalesceInput(data, pane_id) => {
                        // Track if input contains \r (command execution).
                        if data.contains(&b'\r') {
                            pending_input_is_command = true;
                        }
                        if pending_input_pane.is_none() {
                            pending_input_pane = pane_id;
                        }
                        pending_input.extend_from_slice(&data);
                        last_input_mono_ns = event.mono_ns;
                    }
                    RequestDslResult::Skip => {}
                }
            }
        }
    }

    // Flush any remaining pending input.
    if !pending_input.is_empty() {
        flush_input(&mut lines, &pending_input, pending_input_pane, &state);
        if pending_input_is_command && !output_accum.is_empty() {
            generate_assertions_from_output(&mut lines, &output_accum, &state);
        }
    } else if !output_accum.is_empty() {
        generate_assertions_from_output(&mut lines, &output_accum, &state);
    }

    lines.push(String::new());

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Input flushing
// ---------------------------------------------------------------------------

/// Emit a `send-keys` line, optionally with pane targeting.
fn flush_input(
    lines: &mut Vec<String>,
    data: &[u8],
    pane_id: Option<Uuid>,
    state: &RecordingStateTracker,
) {
    let escaped = bytes_to_c_escaped(data);

    // Determine if we need explicit pane targeting.
    let pane_arg = pane_id.and_then(|id| {
        let target_idx = state.pane_index(&id)?;
        let focused_idx = state.focused_pane_index();
        // Only add pane arg if target differs from the currently focused pane
        // and there are multiple panes.
        if state.pane_uuid_to_index.len() > 1 && (focused_idx != Some(target_idx)) {
            Some(target_idx)
        } else {
            None
        }
    });

    match pane_arg {
        Some(idx) => lines.push(format!("send-keys keys='{escaped}' pane={idx}")),
        None => lines.push(format!("send-keys keys='{escaped}'")),
    }
}

// ---------------------------------------------------------------------------
// Assertion generation from output
// ---------------------------------------------------------------------------

/// Generate `wait-for` barriers and `assert-screen` checks from accumulated
/// output bytes, using vt100 parsing to extract the rendered screen content.
fn generate_assertions_from_output(
    lines: &mut Vec<String>,
    output_accum: &[PaneOutputAccumulator],
    state: &RecordingStateTracker,
) {
    let (cols, rows) = state.viewport;

    for accum in output_accum {
        if accum.bytes.is_empty() {
            continue;
        }

        let pane_index = state.pane_index(&accum.pane_id);

        // Parse the output through a vt100 terminal emulator.
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(&accum.bytes);
        let screen = parser.screen();

        // Extract visible text lines.
        let mut text_lines: Vec<String> = Vec::new();
        for row in 0..rows {
            let line = screen.contents_between(row, 0, row, cols);
            text_lines.push(line);
        }

        // Find the last non-empty line (often a shell prompt).
        let last_nonempty = text_lines.iter().rposition(|l| !l.trim().is_empty());

        let Some(last_idx) = last_nonempty else {
            continue; // All empty — nothing to assert.
        };

        let prompt_line = text_lines[last_idx].trim();

        // Generate a `wait-for` using the last non-empty line as a regex anchor.
        // We escape regex meta-characters and replace digit sequences with \d+
        // to tolerate non-deterministic numeric content (PIDs, counters, etc.).
        let pattern = make_robust_pattern(prompt_line);

        if !pattern.is_empty() {
            let pane_suffix = pane_index
                .filter(|_| state.pane_uuid_to_index.len() > 1)
                .map_or(String::new(), |idx| format!(" pane={idx}"));
            lines.push(format!("wait-for pattern='{pattern}'{pane_suffix}"));
        }

        // Look for distinctive content lines above the prompt to generate
        // `assert-screen contains=` checks. We pick lines that look like
        // meaningful output (not just whitespace or very short).
        let content_lines = &text_lines[..last_idx];
        let mut assertions_added = 0;
        for content_line in content_lines.iter().rev() {
            let trimmed = content_line.trim();
            if trimmed.is_empty() || trimmed.len() < 3 {
                continue;
            }
            // Skip lines that are purely numeric or look like timing/noise.
            if trimmed
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == ':' || c == ' ')
            {
                continue;
            }
            // Use the literal content for assert-screen (no regex needed).
            let escaped_content = escape_single_quote(trimmed);
            let pane_suffix = pane_index
                .filter(|_| state.pane_uuid_to_index.len() > 1)
                .map_or(String::new(), |idx| format!(" pane={idx}"));
            lines.push(format!(
                "assert-screen contains='{escaped_content}'{pane_suffix}"
            ));
            assertions_added += 1;
            if assertions_added >= 3 {
                break; // Limit assertions per response window.
            }
        }
    }
}

/// Build a regex pattern from a screen line, making it robust to non-deterministic
/// content while preserving structural anchors.
fn make_robust_pattern(line: &str) -> String {
    let mut result = String::new();
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            // Collapse consecutive digits into \d+
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            result.push_str("\\d+");
        } else if is_regex_meta(ch) {
            result.push('\\');
            result.push(ch);
        } else {
            result.push(ch);
        }
    }

    result
}

/// Check if a character is a regex metacharacter that needs escaping.
const fn is_regex_meta(ch: char) -> bool {
    matches!(
        ch,
        '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
    )
}

/// Escape single quotes for DSL string values.
pub(super) fn escape_single_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

// ---------------------------------------------------------------------------
// Request → DSL conversion
// ---------------------------------------------------------------------------

/// Result of converting a Request to a DSL line.
enum RequestDslResult {
    /// A complete DSL line to emit.
    Line(String),
    /// Input bytes to coalesce with subsequent `AttachInput` events.
    /// Second element is the pane UUID the input was sent to (if known).
    CoalesceInput(Vec<u8>, Option<Uuid>),
    /// Skip this request (non-structural).
    Skip,
}

/// Convert a `Request` variant to a DSL action line, if applicable.
fn request_to_dsl(
    request: &Request,
    has_session: &mut bool,
    request_kind: &str,
    state: &RecordingStateTracker,
    event: &RecordingEventEnvelope,
) -> RequestDslResult {
    match request {
        Request::NewSession { name } => {
            *has_session = true;
            RequestDslResult::Line(name.as_ref().map_or_else(
                || "new-session".to_string(),
                |n| format!("new-session name='{n}'"),
            ))
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
            bmux_ipc::SessionSelector::ById(_) => {
                RequestDslResult::Line("# kill-session (by id, manual edit needed)".to_string())
            }
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
            // Use pane_id from the recording envelope if available (Phase 1.1),
            // otherwise fall back to the tracker's focused pane.
            let pane_id = event.pane_id.or(state.focused_pane_id);
            RequestDslResult::CoalesceInput(data.clone(), pane_id)
        }
        Request::PaneDirectInput { data, pane_id, .. } => {
            if data.is_empty() || !*has_session {
                return RequestDslResult::Skip;
            }
            RequestDslResult::CoalesceInput(data.clone(), Some(*pane_id))
        }
        // Skip high-frequency / non-structural requests.
        // Recording-related requests aren't playbook actions.
        // Attach lifecycle is handled implicitly by the playbook engine.
        Request::AttachOutput { .. }
        | Request::AttachSnapshot { .. }
        | Request::AttachPaneSnapshot { .. }
        | Request::AttachLayout { .. }
        | Request::AttachPaneOutputBatch { .. }
        | Request::Ping
        | Request::Hello { .. }
        | Request::HelloV2 { .. }
        | Request::WhoAmI
        | Request::WhoAmIPrincipal
        | Request::ListSessions
        | Request::ListPanes { .. }
        | Request::ListClients
        | Request::SubscribeEvents
        | Request::PollEvents { .. }
        | Request::RecordingStart { .. }
        | Request::RecordingStop { .. }
        | Request::RecordingStatus
        | Request::RecordingList
        | Request::RecordingDelete { .. }
        | Request::RecordingDeleteAll
        | Request::RecordingWriteCustomEvent { .. }
        | Request::Attach { .. }
        | Request::AttachContext { .. }
        | Request::AttachOpen { .. }
        | Request::Detach => RequestDslResult::Skip,
        // Everything else gets a comment for manual review.
        _ => RequestDslResult::Line(format!("# unhandled request: {request_kind}")),
    }
}

// ---------------------------------------------------------------------------
// Byte escaping
// ---------------------------------------------------------------------------

/// Escape bytes to C-style escape string for use in `send-keys keys='...'`.
pub(super) fn bytes_to_c_escaped(data: &[u8]) -> String {
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
                write!(result, "\\x{byte:02x}").unwrap();
            }
            0x7f => result.push_str("\\x7f"),
            0x20..=0x7e => result.push(byte as char),
            _ => {
                write!(result, "\\x{byte:02x}").unwrap();
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    #[test]
    fn make_robust_pattern_digits() {
        assert_eq!(make_robust_pattern("pid: 12345"), "pid: \\d+");
        assert_eq!(make_robust_pattern("line 42: error"), "line \\d+: error");
    }

    #[test]
    fn make_robust_pattern_escapes_meta() {
        assert_eq!(make_robust_pattern("file.txt"), "file\\.txt");
        assert_eq!(make_robust_pattern("a+b"), "a\\+b");
        assert_eq!(make_robust_pattern("user@host:~$"), "user@host:~\\$");
    }

    #[test]
    fn make_robust_pattern_preserves_text() {
        assert_eq!(make_robust_pattern("hello world"), "hello world");
    }

    #[test]
    fn escape_single_quote_basic() {
        assert_eq!(escape_single_quote("it's"), "it\\'s");
        assert_eq!(escape_single_quote("a\\b"), "a\\\\b");
    }

    #[test]
    fn tracker_pane_lifecycle() {
        let mut tracker = RecordingStateTracker::new();
        let pane1 = Uuid::nil();
        let pane2 = Uuid::from_u128(1);

        tracker.add_pane(pane1);
        tracker.set_focus(pane1);
        assert_eq!(tracker.pane_index(&pane1), Some(0));
        assert_eq!(tracker.focused_pane_index(), Some(0));

        tracker.add_pane(pane2);
        tracker.set_focus(pane2);
        assert_eq!(tracker.pane_index(&pane2), Some(1));
        assert_eq!(tracker.focused_pane_index(), Some(1));

        tracker.remove_pane(&pane1);
        assert_eq!(tracker.pane_index(&pane1), None);
        assert_eq!(tracker.pane_index(&pane2), Some(1)); // index preserved
    }

    #[test]
    fn generate_assertions_basic() {
        let mut state = RecordingStateTracker::new();
        let pane_id = Uuid::nil();
        state.add_pane(pane_id);
        state.set_focus(pane_id);
        state.viewport = (40, 10);

        // Simulate output: "hello world\r\nuser@host:~$ "
        let output = b"hello world\r\nuser@host:~$ ";
        let accum = vec![PaneOutputAccumulator {
            pane_id,
            bytes: output.to_vec(),
        }];

        let mut lines = Vec::new();
        generate_assertions_from_output(&mut lines, &accum, &state);

        // Should have a wait-for on the prompt line
        assert!(
            lines.iter().any(|l| l.starts_with("wait-for")),
            "expected wait-for, got: {lines:?}"
        );
        // The prompt pattern should contain "user@host"
        let waitfor = lines.iter().find(|l| l.starts_with("wait-for")).unwrap();
        assert!(
            waitfor.contains("user@host"),
            "wait-for should match prompt: {waitfor}"
        );
    }

    /// Helper to build a synthetic recording event.
    fn make_event(
        seq: u64,
        mono_ns: u64,
        kind: RecordingEventKind,
        payload: RecordingPayload,
        pane_id: Option<Uuid>,
        session_id: Option<Uuid>,
    ) -> RecordingEventEnvelope {
        RecordingEventEnvelope {
            seq,
            mono_ns,
            wall_epoch_ms: 0,
            session_id,
            pane_id,
            client_id: None,
            kind,
            payload,
        }
    }

    /// Helper: encode a Request as binary bytes.
    fn encode_request(req: &bmux_ipc::Request) -> Vec<u8> {
        bmux_ipc::encode(req).unwrap_or_default()
    }

    /// Helper: encode a `ResponsePayload` as binary bytes.
    fn encode_response(resp: &bmux_ipc::ResponsePayload) -> Vec<u8> {
        bmux_ipc::encode(resp).unwrap_or_default()
    }

    #[test]
    fn events_to_playbook_generates_wait_for() {
        let session_id = Uuid::from_u128(1);
        let pane_id = Uuid::from_u128(2);

        // Event sequence: NewSession request → RequestDone with session created →
        // AttachInput (send "echo hi\r") → PaneOutputRaw with response.
        let new_session_req = bmux_ipc::Request::NewSession { name: None };
        let new_session_resp = bmux_ipc::ResponsePayload::SessionCreated {
            id: session_id,
            name: None,
        };
        let attach_input_req = bmux_ipc::Request::AttachInput {
            session_id,
            data: b"echo hi\r".to_vec(),
        };

        let events = vec![
            // NewSession RequestStart
            make_event(
                1,
                100_000_000,
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart {
                    request_id: 1,
                    request_kind: "new_session".to_string(),
                    exclusive: false,
                    request_data: encode_request(&new_session_req),
                },
                None,
                Some(session_id),
            ),
            // NewSession RequestDone
            make_event(
                2,
                200_000_000,
                RecordingEventKind::RequestDone,
                RecordingPayload::RequestDone {
                    request_id: 1,
                    request_kind: "new_session".to_string(),
                    response_kind: "session_created".to_string(),
                    elapsed_ms: 100,
                    request_data: encode_request(&new_session_req),
                    response_data: encode_response(&new_session_resp),
                },
                None,
                Some(session_id),
            ),
            // AttachInput RequestStart
            make_event(
                3,
                1_000_000_000,
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart {
                    request_id: 2,
                    request_kind: "attach_input".to_string(),
                    exclusive: false,
                    request_data: encode_request(&attach_input_req),
                },
                Some(pane_id),
                Some(session_id),
            ),
            // PaneOutputRaw — simulated shell output
            make_event(
                4,
                1_500_000_000,
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"echo hi\r\nhi\r\nuser@host:~$ ".to_vec(),
                },
                Some(pane_id),
                Some(session_id),
            ),
        ];

        let dsl = events_to_playbook(&events);

        assert!(
            dsl.contains("new-session"),
            "should contain new-session: {dsl}"
        );
        assert!(dsl.contains("send-keys"), "should contain send-keys: {dsl}");
        assert!(dsl.contains("wait-for"), "should contain wait-for: {dsl}");
    }

    #[test]
    fn events_to_playbook_pane_targeting() {
        let session_id = Uuid::from_u128(1);
        let pane0 = Uuid::from_u128(10);
        let pane1 = Uuid::from_u128(11);

        let new_session_req = bmux_ipc::Request::NewSession { name: None };
        let new_session_resp = bmux_ipc::ResponsePayload::SessionCreated {
            id: session_id,
            name: None,
        };

        // Use PaneSplit response to register pane1.
        let split_resp = bmux_ipc::ResponsePayload::PaneSplit {
            id: pane1,
            session_id,
        };

        let input_to_pane1 = bmux_ipc::Request::PaneDirectInput {
            session_id,
            pane_id: pane1,
            data: b"echo pane1\r".to_vec(),
        };

        let events = vec![
            // NewSession
            make_event(
                1,
                100_000_000,
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart {
                    request_id: 1,
                    request_kind: "new_session".to_string(),
                    exclusive: false,
                    request_data: encode_request(&new_session_req),
                },
                None,
                Some(session_id),
            ),
            make_event(
                2,
                200_000_000,
                RecordingEventKind::RequestDone,
                RecordingPayload::RequestDone {
                    request_id: 1,
                    request_kind: "new_session".to_string(),
                    response_kind: "session_created".to_string(),
                    elapsed_ms: 100,
                    request_data: encode_request(&new_session_req),
                    response_data: encode_response(&new_session_resp),
                },
                None,
                Some(session_id),
            ),
            // PaneSplit response — registers pane0 (auto) and pane1
            make_event(
                3,
                300_000_000,
                RecordingEventKind::RequestDone,
                RecordingPayload::RequestDone {
                    request_id: 2,
                    request_kind: "split_pane".to_string(),
                    response_kind: "pane_split".to_string(),
                    elapsed_ms: 10,
                    request_data: vec![],
                    response_data: encode_response(&split_resp),
                },
                None,
                Some(session_id),
            ),
            // Input to pane0 (via envelope pane_id) — registers pane0
            make_event(
                4,
                1_000_000_000,
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"$ ".to_vec(),
                },
                Some(pane0),
                Some(session_id),
            ),
            // PaneDirectInput to pane1 (not focused)
            make_event(
                5,
                2_000_000_000,
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart {
                    request_id: 4,
                    request_kind: "pane_direct_input".to_string(),
                    exclusive: false,
                    request_data: encode_request(&input_to_pane1),
                },
                Some(pane1),
                Some(session_id),
            ),
        ];

        let dsl = events_to_playbook(&events);

        // The input to pane1 should have pane targeting
        assert!(dsl.contains("send-keys"), "should have send-keys: {dsl}");
    }

    #[test]
    fn events_to_playbook_viewport_directive() {
        let session_id = Uuid::from_u128(1);

        let new_session_req = bmux_ipc::Request::NewSession { name: None };
        let viewport_req = bmux_ipc::Request::AttachSetViewport {
            session_id,
            cols: 120,
            rows: 40,
            status_top_inset: 0,
            status_bottom_inset: 0,
            cell_pixel_width: 0,
            cell_pixel_height: 0,
        };

        let events = vec![
            make_event(
                1,
                100_000_000,
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart {
                    request_id: 1,
                    request_kind: "new_session".to_string(),
                    exclusive: false,
                    request_data: encode_request(&new_session_req),
                },
                None,
                Some(session_id),
            ),
            make_event(
                2,
                200_000_000,
                RecordingEventKind::RequestStart,
                RecordingPayload::RequestStart {
                    request_id: 2,
                    request_kind: "attach_set_viewport".to_string(),
                    exclusive: false,
                    request_data: encode_request(&viewport_req),
                },
                None,
                Some(session_id),
            ),
        ];

        let dsl = events_to_playbook(&events);

        assert!(
            dsl.contains("@viewport cols=120 rows=40"),
            "should contain viewport directive: {dsl}"
        );
    }
}
