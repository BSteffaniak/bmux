#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

pub mod notification_output;
pub mod pointer_output;
pub mod pointer_shape;

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
// The trace buffer a caller shares (`with_trace`), locked only around a push.
// simulation-safe: never held across a switch point
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

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

/// Answers the queries a program in a pane sends its terminal. A terminal parser delimits the
/// sequences (the pane's grid does, in the same pass that applies them) and hands each here with
/// the state it is answered from; the engine keeps only what no grid tracks (the kitty keyboard
/// flag stack).
#[derive(Debug)]
pub struct TerminalProtocolEngine {
    profile: ProtocolProfile,
    pane_id: Option<u16>,
    trace: Option<SharedProtocolTraceBuffer>,
    bracketed_paste_supported: bool,
    /// Stack of kitty keyboard enhancement flags pushed by the inner program.
    #[cfg(feature = "kitty-keyboard")]
    keyboard_flag_stack: Vec<u32>,
}

impl TerminalProtocolEngine {
    #[must_use]
    pub const fn new(profile: ProtocolProfile) -> Self {
        Self {
            profile,
            pane_id: None,
            trace: None,
            bracketed_paste_supported: true,
            #[cfg(feature = "kitty-keyboard")]
            keyboard_flag_stack: Vec::new(),
        }
    }

    #[must_use]
    pub const fn with_bracketed_paste_support(mut self, supported: bool) -> Self {
        self.bracketed_paste_supported = supported;
        self
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

    /// Whether a CSI sequence with this final byte can be a query [`Self::answer_csi`] answers:
    /// a parser can skip building the rest for any other.
    #[must_use]
    pub const fn may_answer_csi(action: char) -> bool {
        matches!(action, 'c' | 'n' | 'p' | 'u')
    }

    /// Answer a CSI sequence, appending any reply to `replies`. `values` are its parameter values
    /// (an omitted one is 0), `intermediates` its private marker and intermediate bytes in order;
    /// `cursor` is the cursor (zero-based row and column) as of the sequence, `bracketed_paste`
    /// whether the program has bracketed paste on, and `synchronized_update` whether the program
    /// currently has DEC mode 2026 active.
    #[allow(clippy::too_many_arguments)]
    pub fn answer_csi(
        &mut self,
        values: &[u16],
        intermediates: &[u8],
        action: char,
        cursor: (u16, u16),
        bracketed_paste: bool,
        synchronized_update: bool,
        replies: &mut Vec<u8>,
    ) {
        #[cfg(feature = "kitty-keyboard")]
        if action == 'u'
            && let Some((name, reply)) = self.handle_kitty_keyboard_csi(values, intermediates)
        {
            self.trace_csi(name, values, intermediates, action, &reply);
            replies.extend_from_slice(&reply);
            return;
        }
        let bracketed_paste = self.bracketed_paste_supported && bracketed_paste;
        if let Some((name, reply)) = csi_query_reply(
            values,
            intermediates,
            action,
            cursor,
            self.profile,
            bracketed_paste,
            self.bracketed_paste_supported,
            synchronized_update,
        ) {
            self.trace_csi(name, values, intermediates, action, &reply);
            replies.extend_from_slice(&reply);
        }
    }

    /// Answer an OSC string (split at `;`), appending any reply to `replies`.
    pub fn answer_osc(&self, params: &[&[u8]], replies: &mut Vec<u8>) {
        if let Some((name, reply)) = osc_query_reply(params, self.profile) {
            if self.trace.is_some() {
                let query = params.join(&b';');
                self.trace_event("osc", name, ProtocolDirection::Query, &query);
                self.trace_event("osc", name, ProtocolDirection::Reply, &reply);
            }
            replies.extend_from_slice(&reply);
        }
    }

    /// Whether a DCS string with this introducer is a query [`Self::answer_dcs`] answers, so its
    /// data should be collected.
    #[must_use]
    pub fn wants_dcs(values: &[u16], intermediates: &[u8], action: char) -> bool {
        action == 'q' && matches!(intermediates, b"+" | b"$") && is_default(values)
    }

    /// Answer a complete DCS string [`Self::wants_dcs`] accepted, appending the reply.
    pub fn answer_dcs(&self, intermediates: &[u8], data: &[u8], replies: &mut Vec<u8>) {
        let (name, reply) = match intermediates {
            b"+" => ("dcs_xtgettcap_query", xtgettcap_reply(data, self.profile)),
            b"$" => ("dcs_decrqss_query", decrqss_reply(data)),
            _ => return,
        };
        if self.trace.is_some() {
            let mut query = intermediates.to_vec();
            query.push(b'q');
            query.extend_from_slice(data);
            self.trace_event("dcs", name, ProtocolDirection::Query, &query);
            self.trace_event("dcs", name, ProtocolDirection::Reply, &reply);
        }
        replies.extend_from_slice(&reply);
    }

    /// Trace a CSI query and its reply, the query written back as its bytes would read.
    fn trace_csi(
        &self,
        name: &str,
        values: &[u16],
        intermediates: &[u8],
        action: char,
        reply: &[u8],
    ) {
        if self.trace.is_none() {
            return;
        }
        let (private, rest) = match intermediates.first() {
            Some(marker @ b'<'..=b'?') => (std::slice::from_ref(marker), &intermediates[1..]),
            _ => (&[][..], intermediates),
        };
        let mut query = private.to_vec();
        if !is_default(values) {
            let joined: Vec<String> = values.iter().map(u16::to_string).collect();
            query.extend_from_slice(joined.join(";").as_bytes());
        }
        query.extend_from_slice(rest);
        query.push(u8::try_from(action).unwrap_or(b'?'));
        self.trace_event("csi", name, ProtocolDirection::Query, &query);
        if !reply.is_empty() {
            self.trace_event("csi", name, ProtocolDirection::Reply, reply);
        }
    }

    fn trace_event(&self, family: &str, name: &str, direction: ProtocolDirection, bytes: &[u8]) {
        let Some(trace) = &self.trace else {
            return;
        };
        let event = ProtocolTraceEvent {
            timestamp_ms: switchy_time::now()
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

    /// Handle kitty keyboard protocol CSI sequences (final byte `u`): `CSI ? u` queries the
    /// flags, `CSI > flags u` pushes them and `CSI < u` pops one level.
    ///
    /// Returns `Some((name, reply_bytes))` if the sequence was recognized.
    /// Push/pop sequences return an empty reply (they are state changes only).
    #[cfg(feature = "kitty-keyboard")]
    fn handle_kitty_keyboard_csi(
        &mut self,
        values: &[u16],
        intermediates: &[u8],
    ) -> Option<(&'static str, Vec<u8>)> {
        // Only Bmux profile responds to kitty keyboard queries.
        if !matches!(self.profile, ProtocolProfile::Bmux) {
            return None;
        }
        match intermediates {
            b"?" if is_default(values) => {
                let flags = self.keyboard_flag_stack.last().copied().unwrap_or(0);
                Some((
                    "csi_kitty_keyboard_query",
                    format!("\x1b[?{flags}u").into_bytes(),
                ))
            }
            b"<" if is_default(values) => {
                self.keyboard_flag_stack.pop();
                Some(("csi_kitty_keyboard_pop", Vec::new()))
            }
            b">" => {
                let flags = match values {
                    [flags] => u32::from(*flags),
                    _ => 0,
                };
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
    let normalized = term.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "bmux" | "bmux-256color" => ProtocolProfile::Bmux,
        "screen" | "screen-256color" | "tmux" | "tmux-256color" => ProtocolProfile::Screen,
        "xterm" | "xterm-color" | "xterm-256color" | "xterm-direct" => ProtocolProfile::Xterm,
        _ => ProtocolProfile::Conservative,
    }
}

/// Whether a sequence's parameters are all omitted (the parser reports that as one 0).
fn is_default(values: &[u16]) -> bool {
    matches!(values, [] | [0])
}

#[allow(clippy::too_many_arguments)]
fn csi_query_reply(
    values: &[u16],
    intermediates: &[u8],
    action: char,
    cursor_pos: (u16, u16),
    profile: ProtocolProfile,
    bracketed_paste: bool,
    bracketed_paste_supported: bool,
    synchronized_update: bool,
) -> Option<(&'static str, Vec<u8>)> {
    match (intermediates, action, values) {
        (b"", 'c', _) if is_default(values) => {
            Some(("csi_primary_da", primary_da_response(profile).to_vec()))
        }
        (b">", 'c', _) if is_default(values) => {
            Some(("csi_secondary_da", secondary_da_response(profile).to_vec()))
        }
        (b"", 'n', [5]) => Some(("csi_dsr_status_report", b"\x1b[0n".to_vec())),
        (b"", 'n', [6]) => Some(("csi_dsr_cursor_position", dsr_cursor_response(cursor_pos))),
        (b"?", 'n', [5]) => Some(("csi_dec_dsr_status_report", b"\x1b[?0n".to_vec())),
        (b"?", 'n', [6]) => Some((
            "csi_dec_dsr_cursor_position",
            dec_dsr_cursor_response(cursor_pos),
        )),
        (b"?$", 'p', _) if !is_default(values) => dec_mode_report_response(
            values,
            profile,
            bracketed_paste,
            bracketed_paste_supported,
            synchronized_update,
        )
        .map(|reply| ("csi_dec_mode_report", reply)),
        _ => None,
    }
}

fn dec_mode_report_response(
    modes: &[u16],
    profile: ProtocolProfile,
    bracketed_paste: bool,
    bracketed_paste_supported: bool,
    synchronized_update: bool,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for &mode in modes {
        let status = if mode == 2004 {
            if !bracketed_paste_supported {
                0
            } else if bracketed_paste {
                1
            } else {
                2
            }
        } else if mode == 2026 {
            if matches!(profile, ProtocolProfile::Conservative) {
                0
            } else if synchronized_update {
                1
            } else {
                2
            }
        } else {
            dec_mode_status(profile, mode)
        };
        out.extend_from_slice(format!("\x1b[?{mode};{status}$y").as_bytes());
    }
    (!out.is_empty()).then_some(out)
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

fn osc_query_reply(params: &[&[u8]], profile: ProtocolProfile) -> Option<(&'static str, Vec<u8>)> {
    match params {
        [b"10", b"?"] => Some((
            "osc_color_query",
            format!("\x1b]10;{}\x1b\\", osc_foreground_color(profile)).into_bytes(),
        )),
        [b"11", b"?"] => Some((
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

    /// The reply to a CSI sequence with these intermediates, values and final byte, with the
    /// cursor at the origin and bracketed paste as given.
    fn csi(
        engine: &mut TerminalProtocolEngine,
        intermediates: &[u8],
        values: &[u16],
        action: char,
        bracketed_paste: bool,
    ) -> Vec<u8> {
        let mut replies = Vec::new();
        engine.answer_csi(
            values,
            intermediates,
            action,
            (0, 0),
            bracketed_paste,
            false,
            &mut replies,
        );
        replies
    }

    #[test]
    fn protocol_profile_for_term_recognizes_common_aliases() {
        assert_eq!(protocol_profile_for_term("bmux"), ProtocolProfile::Bmux);
        assert_eq!(protocol_profile_for_term("tmux"), ProtocolProfile::Screen);
        assert_eq!(protocol_profile_for_term("xterm"), ProtocolProfile::Xterm);
        assert_eq!(
            protocol_profile_for_term("xterm-direct"),
            ProtocolProfile::Xterm
        );
        assert_eq!(
            protocol_profile_for_term("weird-term"),
            ProtocolProfile::Conservative
        );
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_push_pop_and_query() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        assert_eq!(csi(&mut engine, b"?", &[0], 'u', false), b"\x1b[?0u");
        // Pushes and pops are state changes, with no reply.
        assert!(csi(&mut engine, b">", &[1], 'u', false).is_empty());
        assert_eq!(csi(&mut engine, b"?", &[0], 'u', false), b"\x1b[?1u");
        assert!(csi(&mut engine, b">", &[3], 'u', false).is_empty());
        assert_eq!(engine.keyboard_enhancement_flags(), 3);
        assert!(csi(&mut engine, b"<", &[0], 'u', false).is_empty());
        assert_eq!(csi(&mut engine, b"?", &[0], 'u', false), b"\x1b[?1u");
        assert!(csi(&mut engine, b"<", &[0], 'u', false).is_empty());
        // Popping an empty stack does nothing.
        assert!(csi(&mut engine, b"<", &[0], 'u', false).is_empty());
        assert_eq!(engine.keyboard_enhancement_flags(), 0);
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn kitty_keyboard_is_answered_only_on_the_bmux_profile() {
        for profile in [ProtocolProfile::Xterm, ProtocolProfile::Conservative] {
            let mut engine = TerminalProtocolEngine::new(profile);
            assert!(csi(&mut engine, b"?", &[0], 'u', false).is_empty());
        }
    }

    #[test]
    fn dec_mode_reports_follow_the_profile() {
        let report = |profile, modes: &[u16]| {
            csi(
                &mut TerminalProtocolEngine::new(profile),
                b"?$",
                modes,
                'p',
                false,
            )
        };
        // Mouse tracking (1000) is reset-but-supported where the profile offers it, and unknown
        // to the screen and conservative profiles.
        for (profile, reply) in [
            (ProtocolProfile::Bmux, &b"\x1b[?1000;2$y"[..]),
            (ProtocolProfile::Xterm, b"\x1b[?1000;2$y"),
            (ProtocolProfile::Screen, b"\x1b[?1000;0$y"),
            (ProtocolProfile::Conservative, b"\x1b[?1000;0$y"),
        ] {
            assert_eq!(report(profile, &[1000]), reply, "{profile:?}");
        }
        // Application cursor keys (1) are unknown only to the conservative profile.
        assert_eq!(
            report(ProtocolProfile::Xterm, &[1, 25]),
            b"\x1b[?1;2$y\x1b[?25;1$y"
        );
        assert_eq!(
            report(ProtocolProfile::Conservative, &[1, 25]),
            b"\x1b[?1;0$y\x1b[?25;1$y"
        );
        // In-band resize notifications (2048) are not offered by any profile; the kitty
        // keyboard protocol is negotiated with CSI ? u, not this mode.
        for profile in [ProtocolProfile::Bmux, ProtocolProfile::Xterm] {
            assert_eq!(report(profile, &[2048]), b"\x1b[?2048;0$y", "{profile:?}");
        }
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        // A report with no mode is no query.
        assert!(csi(&mut engine, b"?$", &[0], 'p', false).is_empty());
    }

    #[test]
    fn dec_mode_2026_reports_synchronized_update_state() {
        for profile in [
            ProtocolProfile::Bmux,
            ProtocolProfile::Xterm,
            ProtocolProfile::Screen,
        ] {
            let mut engine = TerminalProtocolEngine::new(profile);
            let mut replies = Vec::new();
            engine.answer_csi(&[2026], b"?$", 'p', (0, 0), false, false, &mut replies);
            assert_eq!(replies, b"\x1b[?2026;2$y");
            replies.clear();
            engine.answer_csi(&[2026], b"?$", 'p', (0, 0), false, true, &mut replies);
            assert_eq!(replies, b"\x1b[?2026;1$y");
        }
        let mut conservative = TerminalProtocolEngine::new(ProtocolProfile::Conservative);
        let mut replies = Vec::new();
        conservative.answer_csi(&[2026], b"?$", 'p', (0, 0), false, true, &mut replies);
        assert_eq!(replies, b"\x1b[?2026;0$y");
    }

    #[test]
    fn device_attributes_and_status_reports_reply() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        assert_eq!(csi(&mut engine, b"", &[0], 'c', false), b"\x1b[?1;2c");
        assert_eq!(csi(&mut engine, b">", &[0], 'c', false), b"\x1b[>84;0;0c");
        assert_eq!(csi(&mut engine, b"", &[5], 'n', false), b"\x1b[0n");
        assert_eq!(csi(&mut engine, b"?", &[5], 'n', false), b"\x1b[?0n");
        let mut replies = Vec::new();
        engine.answer_csi(&[6], b"", 'n', (4, 9), false, false, &mut replies);
        engine.answer_csi(&[6], b"?", 'n', (4, 9), false, false, &mut replies);
        assert_eq!(replies, b"\x1b[5;10R\x1b[?5;10R");
        // Other sequences are no queries.
        assert!(csi(&mut engine, b"", &[1], 'c', false).is_empty());
        assert!(csi(&mut engine, b"", &[31], 'm', false).is_empty());
    }

    #[cfg(feature = "kitty-keyboard")]
    #[test]
    fn supported_query_names_includes_kitty() {
        let names = supported_query_names();
        assert!(names.contains(&"csi_kitty_keyboard_query"));
        assert!(names.contains(&"csi_kitty_keyboard_push"));
        assert!(names.contains(&"csi_kitty_keyboard_pop"));
    }

    #[test]
    fn dec_mode_2004_reports_the_programs_bracketed_paste() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        assert_eq!(
            csi(&mut engine, b"?$", &[2004], 'p', false),
            b"\x1b[?2004;2$y"
        );
        assert_eq!(
            csi(&mut engine, b"?$", &[2004], 'p', true),
            b"\x1b[?2004;1$y"
        );
        let mut unsupported =
            TerminalProtocolEngine::new(ProtocolProfile::Bmux).with_bracketed_paste_support(false);
        assert_eq!(
            csi(&mut unsupported, b"?$", &[2004], 'p', true),
            b"\x1b[?2004;0$y"
        );
    }

    #[test]
    fn color_and_string_queries_reply() {
        let engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut replies = Vec::new();
        engine.answer_osc(&[b"11", b"?"], &mut replies);
        engine.answer_osc(&[b"11", b"rgb:0/0/0"], &mut replies);
        assert_eq!(replies, b"\x1b]11;rgb:1a1a/1a1a/1a1a\x1b\\");
        assert!(TerminalProtocolEngine::wants_dcs(&[0], b"+", 'q'));
        assert!(!TerminalProtocolEngine::wants_dcs(&[0], b"", 'q'));
        replies.clear();
        engine.answer_dcs(b"+", b"636f", &mut replies);
        engine.answer_dcs(b"$", b"m", &mut replies);
        assert_eq!(replies, b"\x1bP1+r636f=323536\x1b\\\x1bP1$r0m\x1b\\");
    }
}
