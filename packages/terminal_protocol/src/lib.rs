#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    Dcs,
    DcsEsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolProfile {
    Bmux,
    Xterm,
    Screen,
    Conservative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolDirection {
    Query,
    Reply,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolTraceEvent {
    pub timestamp_ms: u128,
    pub pane_id: Option<u16>,
    pub profile: String,
    pub family: String,
    pub name: String,
    pub direction: ProtocolDirection,
    pub raw_hex: String,
    pub decoded: String,
}

#[derive(Debug, Default)]
pub struct ProtocolTraceBuffer {
    capacity: usize,
    dropped: usize,
    events: VecDeque<ProtocolTraceEvent>,
}

impl ProtocolTraceBuffer {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            dropped: 0,
            events: VecDeque::new(),
        }
    }

    fn push(&mut self, event: ProtocolTraceEvent) {
        if self.events.len() == self.capacity {
            let _ = self.events.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.events.push_back(event);
    }

    #[must_use]
    pub fn snapshot(&self, limit: usize) -> Vec<ProtocolTraceEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let len = self.events.len();
        let start = len.saturating_sub(limit);
        self.events.iter().skip(start).cloned().collect()
    }

    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }
}

pub type SharedProtocolTraceBuffer = Arc<Mutex<ProtocolTraceBuffer>>;

#[derive(Debug)]
pub struct TerminalProtocolEngine {
    state: ParseState,
    csi_buffer: Vec<u8>,
    osc_buffer: Vec<u8>,
    dcs_buffer: Vec<u8>,
    profile: ProtocolProfile,
    pane_id: Option<u16>,
    trace: Option<SharedProtocolTraceBuffer>,
    /// Stack of kitty keyboard enhancement flags pushed by the inner program.
    #[cfg(feature = "kitty-keyboard")]
    keyboard_flag_stack: Vec<u32>,
}

impl TerminalProtocolEngine {
    #[must_use]
    pub fn new(profile: ProtocolProfile) -> Self {
        Self {
            state: ParseState::Ground,
            csi_buffer: Vec::new(),
            osc_buffer: Vec::new(),
            dcs_buffer: Vec::new(),
            profile,
            pane_id: None,
            trace: None,
            #[cfg(feature = "kitty-keyboard")]
            keyboard_flag_stack: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_trace(
        profile: ProtocolProfile,
        pane_id: u16,
        trace: SharedProtocolTraceBuffer,
    ) -> Self {
        let mut this = Self::new(profile);
        this.pane_id = Some(pane_id);
        this.trace = Some(trace);
        this
    }

    #[must_use]
    pub fn process_output(&mut self, bytes: &[u8], cursor_pos: (u16, u16)) -> Vec<u8> {
        let mut replies = Vec::new();

        for byte in bytes {
            match self.state {
                ParseState::Ground => {
                    if *byte == 0x1b {
                        self.state = ParseState::Esc;
                    }
                }
                ParseState::Esc => {
                    if *byte == b'[' {
                        self.state = ParseState::Csi;
                        self.csi_buffer.clear();
                    } else if *byte == b']' {
                        self.state = ParseState::Osc;
                        self.osc_buffer.clear();
                    } else if *byte == b'P' {
                        self.state = ParseState::Dcs;
                        self.dcs_buffer.clear();
                    } else if *byte == 0x1b {
                        self.state = ParseState::Esc;
                    } else {
                        self.state = ParseState::Ground;
                    }
                }
                ParseState::Csi => {
                    if *byte == 0x1b {
                        self.state = ParseState::Esc;
                        self.csi_buffer.clear();
                        continue;
                    }

                    self.csi_buffer.push(*byte);

                    if byte.is_ascii_alphabetic() {
                        // First try kitty keyboard protocol sequences (state-changing).
                        #[cfg(feature = "kitty-keyboard")]
                        if *byte == b'u' {
                            if let Some((name, reply)) =
                                self.handle_kitty_keyboard_csi(&self.csi_buffer.clone())
                            {
                                self.trace_event(
                                    "csi",
                                    name,
                                    ProtocolDirection::Query,
                                    &self.csi_buffer,
                                );
                                if !reply.is_empty() {
                                    self.trace_event("csi", name, ProtocolDirection::Reply, &reply);
                                    replies.extend_from_slice(&reply);
                                }
                                self.state = ParseState::Ground;
                                self.csi_buffer.clear();
                                continue;
                            }
                        }

                        if let Some((name, reply)) =
                            csi_query_reply(&self.csi_buffer, cursor_pos, self.profile)
                        {
                            self.trace_event(
                                "csi",
                                name,
                                ProtocolDirection::Query,
                                &self.csi_buffer,
                            );
                            self.trace_event("csi", name, ProtocolDirection::Reply, &reply);
                            replies.extend_from_slice(&reply);
                        }
                        self.state = ParseState::Ground;
                        self.csi_buffer.clear();
                    } else if self.csi_buffer.len() > 32 {
                        self.state = ParseState::Ground;
                        self.csi_buffer.clear();
                    }
                }
                ParseState::Osc => {
                    if *byte == 0x07 {
                        if let Some((name, reply)) = osc_query_reply(&self.osc_buffer, self.profile)
                        {
                            self.trace_event(
                                "osc",
                                name,
                                ProtocolDirection::Query,
                                &self.osc_buffer,
                            );
                            self.trace_event("osc", name, ProtocolDirection::Reply, &reply);
                            replies.extend_from_slice(&reply);
                        }
                        self.state = ParseState::Ground;
                        self.osc_buffer.clear();
                    } else if *byte == 0x1b {
                        self.state = ParseState::OscEsc;
                    } else {
                        self.osc_buffer.push(*byte);
                        if self.osc_buffer.len() > 512 {
                            self.state = ParseState::Ground;
                            self.osc_buffer.clear();
                        }
                    }
                }
                ParseState::OscEsc => {
                    if *byte == b'\\' {
                        if let Some((name, reply)) = osc_query_reply(&self.osc_buffer, self.profile)
                        {
                            self.trace_event(
                                "osc",
                                name,
                                ProtocolDirection::Query,
                                &self.osc_buffer,
                            );
                            self.trace_event("osc", name, ProtocolDirection::Reply, &reply);
                            replies.extend_from_slice(&reply);
                        }
                        self.state = ParseState::Ground;
                        self.osc_buffer.clear();
                    } else {
                        self.state = ParseState::Ground;
                        self.osc_buffer.clear();
                    }
                }
                ParseState::Dcs => {
                    if *byte == 0x1b {
                        self.state = ParseState::DcsEsc;
                    } else {
                        self.dcs_buffer.push(*byte);
                        if self.dcs_buffer.len() > 512 {
                            self.state = ParseState::Ground;
                            self.dcs_buffer.clear();
                        }
                    }
                }
                ParseState::DcsEsc => {
                    if *byte == b'\\' {
                        if let Some((name, reply)) = dcs_query_reply(&self.dcs_buffer, self.profile)
                        {
                            self.trace_event(
                                "dcs",
                                name,
                                ProtocolDirection::Query,
                                &self.dcs_buffer,
                            );
                            self.trace_event("dcs", name, ProtocolDirection::Reply, &reply);
                            replies.extend_from_slice(&reply);
                        }
                        self.state = ParseState::Ground;
                        self.dcs_buffer.clear();
                    } else {
                        self.state = ParseState::Ground;
                        self.dcs_buffer.clear();
                    }
                }
            }
        }

        replies
    }

    fn trace_event(&self, family: &str, name: &str, direction: ProtocolDirection, bytes: &[u8]) {
        let Some(trace) = &self.trace else {
            return;
        };
        let event = ProtocolTraceEvent {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |dur| dur.as_millis()),
            pane_id: self.pane_id,
            profile: protocol_profile_name(self.profile).to_string(),
            family: family.to_string(),
            name: name.to_string(),
            direction,
            raw_hex: bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(""),
            decoded: String::from_utf8_lossy(bytes).into_owned(),
        };
        if let Ok(mut guard) = trace.lock() {
            guard.push(event);
        }
    }

    /// Handle kitty keyboard protocol CSI sequences.
    ///
    /// Returns `Some((name, reply_bytes))` if the sequence was recognized.
    /// Push/pop sequences return an empty reply (they are state changes only).
    #[cfg(feature = "kitty-keyboard")]
    fn handle_kitty_keyboard_csi(&mut self, sequence: &[u8]) -> Option<(&'static str, Vec<u8>)> {
        // Only Bmux profile responds to kitty keyboard queries.
        if !matches!(self.profile, ProtocolProfile::Bmux) {
            return None;
        }

        match sequence {
            // Query: CSI ? u -> reply with current flags
            b"?u" => {
                let flags = self.keyboard_flag_stack.last().copied().unwrap_or(0);
                Some((
                    "csi_kitty_keyboard_query",
                    format!("\x1b[?{flags}u").into_bytes(),
                ))
            }
            // Pop: CSI < u -> pop one level from the stack
            b"<u" => {
                self.keyboard_flag_stack.pop();
                Some(("csi_kitty_keyboard_pop", Vec::new()))
            }
            // Push: CSI > {flags} u -> push flags onto the stack
            _ if sequence.starts_with(b">") && sequence.ends_with(b"u") => {
                let flags_str = &sequence[1..sequence.len() - 1];
                let flags = std::str::from_utf8(flags_str)
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                self.keyboard_flag_stack.push(flags);
                Some(("csi_kitty_keyboard_push", Vec::new()))
            }
            _ => None,
        }
    }

    /// Get the current kitty keyboard enhancement flags for this pane.
    ///
    /// Returns 0 if no flags have been pushed.
    #[cfg(feature = "kitty-keyboard")]
    #[must_use]
    pub fn keyboard_enhancement_flags(&self) -> u32 {
        self.keyboard_flag_stack.last().copied().unwrap_or(0)
    }
}

impl Default for TerminalProtocolEngine {
    fn default() -> Self {
        Self::new(ProtocolProfile::Conservative)
    }
}

#[must_use]
pub fn supported_query_names() -> &'static [&'static str] {
    &[
        "csi_primary_da",
        "csi_secondary_da",
        "csi_dsr_status_report",
        "csi_dsr_cursor_position",
        "csi_dec_dsr_status_report",
        "csi_dec_dsr_cursor_position",
        "csi_dec_mode_report",
        #[cfg(feature = "kitty-keyboard")]
        "csi_kitty_keyboard_query",
        #[cfg(feature = "kitty-keyboard")]
        "csi_kitty_keyboard_push",
        #[cfg(feature = "kitty-keyboard")]
        "csi_kitty_keyboard_pop",
        "osc_color_query",
        "dcs_xtgettcap_query",
        "dcs_decrqss_query",
    ]
}

#[must_use]
pub fn protocol_profile_name(profile: ProtocolProfile) -> &'static str {
    match profile {
        ProtocolProfile::Bmux => "bmux",
        ProtocolProfile::Xterm => "xterm",
        ProtocolProfile::Screen => "screen",
        ProtocolProfile::Conservative => "conservative",
    }
}

#[must_use]
pub fn primary_da_for_profile(profile: ProtocolProfile) -> &'static [u8] {
    primary_da_response(profile)
}

#[must_use]
pub fn secondary_da_for_profile(profile: ProtocolProfile) -> &'static [u8] {
    secondary_da_response(profile)
}

#[must_use]
pub fn protocol_profile_for_term(term: &str) -> ProtocolProfile {
    match term {
        "bmux-256color" => ProtocolProfile::Bmux,
        "screen-256color" | "tmux-256color" => ProtocolProfile::Screen,
        "xterm-256color" => ProtocolProfile::Xterm,
        _ => ProtocolProfile::Conservative,
    }
}

fn csi_query_reply(
    sequence: &[u8],
    cursor_pos: (u16, u16),
    profile: ProtocolProfile,
) -> Option<(&'static str, Vec<u8>)> {
    match sequence {
        b"c" | b"0c" => Some(("csi_primary_da", primary_da_response(profile).to_vec())),
        b">c" => Some(("csi_secondary_da", secondary_da_response(profile).to_vec())),
        b"5n" => Some(("csi_dsr_status_report", b"\x1b[0n".to_vec())),
        b"6n" => Some(("csi_dsr_cursor_position", dsr_cursor_response(cursor_pos))),
        b"?5n" => Some(("csi_dec_dsr_status_report", b"\x1b[?0n".to_vec())),
        b"?6n" => Some((
            "csi_dec_dsr_cursor_position",
            dec_dsr_cursor_response(cursor_pos),
        )),
        _ if sequence.starts_with(b"?") && sequence.ends_with(b"$p") => {
            dec_mode_report_response(sequence, profile).map(|reply| ("csi_dec_mode_report", reply))
        }
        _ => None,
    }
}

fn dec_mode_report_response(sequence: &[u8], profile: ProtocolProfile) -> Option<Vec<u8>> {
    let mode_bytes = &sequence[1..sequence.len().saturating_sub(2)];
    let modes = parse_mode_list(mode_bytes)?;
    let mut out = Vec::new();
    for mode in modes {
        let status = dec_mode_status(profile, mode);
        out.extend_from_slice(format!("\x1b[?{mode};{status}$y").as_bytes());
    }
    (!out.is_empty()).then_some(out)
}

fn parse_mode_list(bytes: &[u8]) -> Option<Vec<u16>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut out = Vec::new();
    for token in text.split(';') {
        if token.is_empty() {
            continue;
        }
        out.push(token.parse::<u16>().ok()?);
    }
    Some(out)
}

fn dec_mode_status(profile: ProtocolProfile, mode: u16) -> u8 {
    match profile {
        ProtocolProfile::Bmux => match mode {
            1 => 2,
            7 => 1,
            25 => 1,
            1000 => 2,
            1002 => 2,
            1006 => 2,
            1004 => 2,
            2004 => 2,
            1049 => 2,
            #[cfg(feature = "kitty-keyboard")]
            2048 => 1, // Kitty keyboard protocol supported
            _ => 0,
        },
        ProtocolProfile::Xterm => match mode {
            1 => 2,
            7 => 1,
            25 => 1,
            1000 => 2,
            1002 => 2,
            1006 => 2,
            1004 => 2,
            2004 => 2,
            1049 => 2,
            _ => 0,
        },
        ProtocolProfile::Screen => match mode {
            1 => 2,
            7 => 1,
            25 => 1,
            1049 => 2,
            _ => 0,
        },
        ProtocolProfile::Conservative => match mode {
            7 => 1,
            25 => 1,
            _ => 0,
        },
    }
}

fn osc_query_reply(sequence: &[u8], profile: ProtocolProfile) -> Option<(&'static str, Vec<u8>)> {
    match sequence {
        b"10;?" => Some((
            "osc_color_query",
            format!("\x1b]10;{}\x1b\\", osc_foreground_color(profile)).into_bytes(),
        )),
        b"11;?" => Some((
            "osc_color_query",
            format!("\x1b]11;{}\x1b\\", osc_background_color(profile)).into_bytes(),
        )),
        _ => None,
    }
}

fn osc_foreground_color(profile: ProtocolProfile) -> &'static str {
    match profile {
        ProtocolProfile::Bmux => "rgb:bfbf/bfbf/bfbf",
        ProtocolProfile::Xterm | ProtocolProfile::Screen => "rgb:ffff/ffff/ffff",
        ProtocolProfile::Conservative => "rgb:c0c0/c0c0/c0c0",
    }
}

fn osc_background_color(profile: ProtocolProfile) -> &'static str {
    match profile {
        ProtocolProfile::Bmux => "rgb:1a1a/1a1a/1a1a",
        ProtocolProfile::Xterm | ProtocolProfile::Screen | ProtocolProfile::Conservative => {
            "rgb:0000/0000/0000"
        }
    }
}

fn dcs_query_reply(sequence: &[u8], profile: ProtocolProfile) -> Option<(&'static str, Vec<u8>)> {
    if let Some(hex_keys) = sequence.strip_prefix(b"+q") {
        return Some(("dcs_xtgettcap_query", xtgettcap_reply(hex_keys, profile)));
    }
    if let Some(request) = sequence.strip_prefix(b"$q") {
        return Some(("dcs_decrqss_query", decrqss_reply(request)));
    }
    None
}

fn xtgettcap_reply(hex_keys: &[u8], profile: ProtocolProfile) -> Vec<u8> {
    let mut parts = Vec::new();
    for key_hex in hex_keys.split(|byte| *byte == b';') {
        if key_hex.is_empty() {
            continue;
        }
        if let Some(value_hex) = xtgettcap_value_hex(key_hex, profile) {
            parts.push(format!(
                "{}={}",
                String::from_utf8_lossy(key_hex),
                String::from_utf8_lossy(value_hex)
            ));
        }
    }

    if parts.is_empty() {
        b"\x1bP0+r\x1b\\".to_vec()
    } else {
        format!("\x1bP1+r{}\x1b\\", parts.join(";"))
            .as_bytes()
            .to_vec()
    }
}

fn xtgettcap_value_hex(key_hex: &[u8], profile: ProtocolProfile) -> Option<&'static [u8]> {
    match key_hex {
        b"5443" => Some(if matches!(profile, ProtocolProfile::Conservative) {
            b"30"
        } else {
            b"31"
        }),
        b"636f" => Some(b"323536"),
        b"6b42" => Some(b"1b5b5a"),
        b"6b44" => Some(b"1b5b44"),
        _ => None,
    }
}

fn decrqss_reply(request: &[u8]) -> Vec<u8> {
    match request {
        b"m" => b"\x1bP1$r0m\x1b\\".to_vec(),
        b" q" => b"\x1bP1$r q\x1b\\".to_vec(),
        _ => b"\x1bP0$r\x1b\\".to_vec(),
    }
}

fn primary_da_response(profile: ProtocolProfile) -> &'static [u8] {
    match profile {
        ProtocolProfile::Bmux | ProtocolProfile::Xterm | ProtocolProfile::Conservative => {
            b"\x1b[?1;2c"
        }
        ProtocolProfile::Screen => b"\x1b[?64;1;2;6;9;15;18;21;22c",
    }
}

fn secondary_da_response(profile: ProtocolProfile) -> &'static [u8] {
    match profile {
        ProtocolProfile::Bmux => b"\x1b[>84;0;0c",
        ProtocolProfile::Xterm => b"\x1b[>0;115;0c",
        ProtocolProfile::Screen => b"\x1b[>83;40003;0c",
        ProtocolProfile::Conservative => b"\x1b[>0;1000;0c",
    }
}

fn dsr_cursor_response(cursor_pos: (u16, u16)) -> Vec<u8> {
    let (row, col) = (
        cursor_pos.0.saturating_add(1),
        cursor_pos.1.saturating_add(1),
    );
    format!("\x1b[{row};{col}R").into_bytes()
}

fn dec_dsr_cursor_response(cursor_pos: (u16, u16)) -> Vec<u8> {
    let (row, col) = (
        cursor_pos.0.saturating_add(1),
        cursor_pos.1.saturating_add(1),
    );
    format!("\x1b[?{row};{col}R").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_query_empty_stack() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let reply = engine.process_output(b"\x1b[?u", (0, 0));
        assert_eq!(reply, b"\x1b[?0u");
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_push_then_query() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        // Push flags=1 (disambiguate escape codes)
        let reply = engine.process_output(b"\x1b[>1u", (0, 0));
        assert!(reply.is_empty(), "push should not produce a reply");
        // Query should return 1
        let reply = engine.process_output(b"\x1b[?u", (0, 0));
        assert_eq!(reply, b"\x1b[?1u");
        assert_eq!(engine.keyboard_enhancement_flags(), 1);
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_push_push_pop_query() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        // Push flags=1
        let _ = engine.process_output(b"\x1b[>1u", (0, 0));
        // Push flags=3
        let _ = engine.process_output(b"\x1b[>3u", (0, 0));
        assert_eq!(engine.keyboard_enhancement_flags(), 3);
        // Pop -> should return to 1
        let _ = engine.process_output(b"\x1b[<u", (0, 0));
        let reply = engine.process_output(b"\x1b[?u", (0, 0));
        assert_eq!(reply, b"\x1b[?1u");
        assert_eq!(engine.keyboard_enhancement_flags(), 1);
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_pop_empty_stack() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        // Pop from empty stack should be a no-op
        let reply = engine.process_output(b"\x1b[<u", (0, 0));
        assert!(reply.is_empty());
        assert_eq!(engine.keyboard_enhancement_flags(), 0);
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_not_supported_on_xterm_profile() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let reply = engine.process_output(b"\x1b[?u", (0, 0));
        assert!(
            reply.is_empty(),
            "xterm profile should not respond to kitty queries"
        );
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_not_supported_on_conservative_profile() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Conservative);
        let reply = engine.process_output(b"\x1b[?u", (0, 0));
        assert!(reply.is_empty());
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn dec_mode_2048_supported_on_bmux_profile() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let reply = engine.process_output(b"\x1b[?2048$p", (0, 0));
        // Mode 2048 should be reported as "1" (set) for Bmux profile.
        assert_eq!(reply, b"\x1b[?2048;1$y");
    }

    #[test]
    fn dec_mode_2048_unknown_on_xterm_profile() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let reply = engine.process_output(b"\x1b[?2048$p", (0, 0));
        // Mode 2048 should be reported as "0" (not recognized) for Xterm profile.
        assert_eq!(reply, b"\x1b[?2048;0$y");
    }

    #[test]
    fn primary_da_replies() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let reply = engine.process_output(b"\x1b[c", (0, 0));
        assert_eq!(reply, b"\x1b[?1;2c");
    }

    #[test]
    fn secondary_da_replies() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let reply = engine.process_output(b"\x1b[>c", (0, 0));
        assert_eq!(reply, b"\x1b[>84;0;0c");
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_interleaved_with_normal_output() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        // Simulate normal output with embedded kitty keyboard push
        let reply = engine.process_output(b"Hello\x1b[>1uWorld", (0, 0));
        assert!(reply.is_empty()); // Push produces no reply
        assert_eq!(engine.keyboard_enhancement_flags(), 1);
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn supported_query_names_includes_kitty() {
        let names = supported_query_names();
        assert!(names.contains(&"csi_kitty_keyboard_query"));
        assert!(names.contains(&"csi_kitty_keyboard_push"));
        assert!(names.contains(&"csi_kitty_keyboard_pop"));
    }
}
