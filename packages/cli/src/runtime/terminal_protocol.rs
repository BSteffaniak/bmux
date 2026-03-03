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
pub(super) enum ProtocolProfile {
    Bmux,
    Xterm,
    Screen,
    Conservative,
}

#[derive(Debug)]
pub(super) struct TerminalProtocolEngine {
    state: ParseState,
    csi_buffer: Vec<u8>,
    osc_buffer: Vec<u8>,
    dcs_buffer: Vec<u8>,
    profile: ProtocolProfile,
}

impl TerminalProtocolEngine {
    pub(super) fn new(profile: ProtocolProfile) -> Self {
        Self {
            state: ParseState::Ground,
            csi_buffer: Vec::new(),
            osc_buffer: Vec::new(),
            dcs_buffer: Vec::new(),
            profile,
        }
    }

    pub(super) fn process_output(&mut self, bytes: &[u8], cursor_pos: (u16, u16)) -> Vec<u8> {
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
                        if let Some(reply) =
                            csi_query_reply(&self.csi_buffer, cursor_pos, self.profile)
                        {
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
                        if let Some(reply) = osc_query_reply(&self.osc_buffer, self.profile) {
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
                        if let Some(reply) = osc_query_reply(&self.osc_buffer, self.profile) {
                            replies.extend_from_slice(&reply);
                        }
                        self.state = ParseState::Ground;
                        self.osc_buffer.clear();
                    } else {
                        self.osc_buffer.push(0x1b);
                        self.osc_buffer.push(*byte);
                        self.state = ParseState::Osc;
                        if self.osc_buffer.len() > 512 {
                            self.state = ParseState::Ground;
                            self.osc_buffer.clear();
                        }
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
                        if let Some(reply) = dcs_query_reply(&self.dcs_buffer, self.profile) {
                            replies.extend_from_slice(&reply);
                        }
                        self.state = ParseState::Ground;
                        self.dcs_buffer.clear();
                    } else {
                        self.dcs_buffer.push(0x1b);
                        self.dcs_buffer.push(*byte);
                        self.state = ParseState::Dcs;
                        if self.dcs_buffer.len() > 512 {
                            self.state = ParseState::Ground;
                            self.dcs_buffer.clear();
                        }
                    }
                }
            }
        }

        replies
    }
}

impl Default for TerminalProtocolEngine {
    fn default() -> Self {
        Self::new(ProtocolProfile::Conservative)
    }
}

pub(super) fn supported_query_names() -> &'static [&'static str] {
    &[
        "csi_primary_da",
        "csi_secondary_da",
        "csi_dsr_status_report",
        "csi_dsr_cursor_position",
        "csi_dec_dsr_status_report",
        "csi_dec_dsr_cursor_position",
        "csi_dec_mode_report",
        "osc_color_query",
        "dcs_xtgettcap_query",
        "dcs_decrqss_query",
    ]
}

fn csi_query_reply(
    sequence: &[u8],
    cursor_pos: (u16, u16),
    profile: ProtocolProfile,
) -> Option<Vec<u8>> {
    match sequence {
        b"c" | b"0c" => Some(primary_da_response(profile).to_vec()),
        b">c" => Some(secondary_da_response(profile).to_vec()),
        b"5n" => Some(b"\x1b[0n".to_vec()),
        b"6n" => Some(dsr_cursor_response(cursor_pos)),
        b"?5n" => Some(b"\x1b[?0n".to_vec()),
        b"?6n" => Some(dec_dsr_cursor_response(cursor_pos)),
        _ if sequence.starts_with(b"?") && sequence.ends_with(b"$p") => {
            dec_mode_report_response(sequence, profile)
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
    Some(out)
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

fn osc_query_reply(sequence: &[u8], profile: ProtocolProfile) -> Option<Vec<u8>> {
    if sequence == b"10;?" {
        return Some(format!("\x1b]10;{}\x1b\\", osc_foreground_color(profile)).into_bytes());
    }
    if sequence == b"11;?" {
        return Some(format!("\x1b]11;{}\x1b\\", osc_background_color(profile)).into_bytes());
    }
    None
}

fn osc_foreground_color(profile: ProtocolProfile) -> &'static str {
    match profile {
        ProtocolProfile::Bmux => "rgb:bfbf/bfbf/bfbf",
        ProtocolProfile::Xterm => "rgb:ffff/ffff/ffff",
        ProtocolProfile::Screen => "rgb:ffff/ffff/ffff",
        ProtocolProfile::Conservative => "rgb:c0c0/c0c0/c0c0",
    }
}

fn osc_background_color(profile: ProtocolProfile) -> &'static str {
    match profile {
        ProtocolProfile::Bmux => "rgb:1a1a/1a1a/1a1a",
        ProtocolProfile::Xterm => "rgb:0000/0000/0000",
        ProtocolProfile::Screen => "rgb:0000/0000/0000",
        ProtocolProfile::Conservative => "rgb:0000/0000/0000",
    }
}

fn dcs_query_reply(sequence: &[u8], profile: ProtocolProfile) -> Option<Vec<u8>> {
    if let Some(hex_keys) = sequence.strip_prefix(b"+q") {
        return Some(xtgettcap_reply(hex_keys, profile));
    }
    if let Some(request) = sequence.strip_prefix(b"$q") {
        return Some(decrqss_reply(request));
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
        }), // TC: 0/1
        b"636f" => Some(b"323536"), // co => 256
        b"6b42" => Some(b"1b5b5a"), // kB => Shift-Tab
        b"6b44" => Some(b"1b5b44"), // kD => Left
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
        ProtocolProfile::Bmux => b"\x1b[?1;2c",
        ProtocolProfile::Xterm => b"\x1b[?1;2c",
        ProtocolProfile::Screen => b"\x1b[?64;1;2;6;9;15;18;21;22c",
        ProtocolProfile::Conservative => b"\x1b[?1;2c",
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

pub(super) fn protocol_profile_name(profile: ProtocolProfile) -> &'static str {
    match profile {
        ProtocolProfile::Bmux => "bmux",
        ProtocolProfile::Xterm => "xterm",
        ProtocolProfile::Screen => "screen",
        ProtocolProfile::Conservative => "conservative",
    }
}

pub(super) fn primary_da_for_profile(profile: ProtocolProfile) -> &'static [u8] {
    primary_da_response(profile)
}

pub(super) fn secondary_da_for_profile(profile: ProtocolProfile) -> &'static [u8] {
    secondary_da_response(profile)
}

fn dsr_cursor_response(cursor_pos: (u16, u16)) -> Vec<u8> {
    let row = u32::from(cursor_pos.0).saturating_add(1);
    let col = u32::from(cursor_pos.1).saturating_add(1);
    format!("\x1b[{row};{col}R").into_bytes()
}

fn dec_dsr_cursor_response(cursor_pos: (u16, u16)) -> Vec<u8> {
    let row = u32::from(cursor_pos.0).saturating_add(1);
    let col = u32::from(cursor_pos.1).saturating_add(1);
    format!("\x1b[?{row};{col}R").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{ProtocolProfile, TerminalProtocolEngine};

    #[test]
    fn replies_to_primary_da_query() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[c", (0, 0));
        assert_eq!(reply, b"\x1b[?1;2c");
    }

    #[test]
    fn replies_to_primary_da_with_zero_param() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[0c", (0, 0));
        assert_eq!(reply, b"\x1b[?1;2c");
    }

    #[test]
    fn handles_split_sequences_across_chunks() {
        let mut engine = TerminalProtocolEngine::default();
        assert!(engine.process_output(b"\x1b[", (0, 0)).is_empty());
        let reply = engine.process_output(b"c", (0, 0));
        assert_eq!(reply, b"\x1b[?1;2c");
    }

    #[test]
    fn replies_to_secondary_da_query() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[>c", (0, 0));
        assert_eq!(reply, b"\x1b[>0;1000;0c");
    }

    #[test]
    fn replies_to_dsr_status_report_query() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[5n", (0, 0));
        assert_eq!(reply, b"\x1b[0n");
    }

    #[test]
    fn replies_to_dsr_cursor_query_with_cursor_position() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[6n", (4, 9));
        assert_eq!(reply, b"\x1b[5;10R");
    }

    #[test]
    fn replies_to_dec_dsr_status_query() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[?5n", (0, 0));
        assert_eq!(reply, b"\x1b[?0n");
    }

    #[test]
    fn replies_to_dec_dsr_cursor_query() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[?6n", (2, 3));
        assert_eq!(reply, b"\x1b[?3;4R");
    }

    #[test]
    fn profile_specific_da_replies_vary() {
        let mut xterm = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut screen = TerminalProtocolEngine::new(ProtocolProfile::Screen);

        assert_eq!(xterm.process_output(b"\x1b[>c", (0, 0)), b"\x1b[>0;115;0c");
        assert_eq!(
            screen.process_output(b"\x1b[>c", (0, 0)),
            b"\x1b[>83;40003;0c"
        );
    }

    #[test]
    fn replies_to_dec_mode_report_query() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let reply = engine.process_output(b"\x1b[?25$p", (0, 0));
        assert_eq!(reply, b"\x1b[?25;1$y");
    }

    #[test]
    fn replies_to_multi_mode_report_query() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let reply = engine.process_output(b"\x1b[?25;1006$p", (0, 0));
        assert_eq!(reply, b"\x1b[?25;1$y\x1b[?1006;2$y");
    }

    #[test]
    fn dec_mode_report_is_profile_gated() {
        let mut xterm = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut conservative = TerminalProtocolEngine::new(ProtocolProfile::Conservative);

        assert_eq!(
            xterm.process_output(b"\x1b[?1006$p", (0, 0)),
            b"\x1b[?1006;2$y"
        );
        assert_eq!(
            conservative.process_output(b"\x1b[?1006$p", (0, 0)),
            b"\x1b[?1006;0$y"
        );
    }

    #[test]
    fn profile_golden_responses_for_common_queries() {
        let mut bmux = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut xterm = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut screen = TerminalProtocolEngine::new(ProtocolProfile::Screen);

        let bmux_reply = bmux.process_output(b"\x1b[c\x1b[>c\x1b[?25$p", (0, 0));
        let xterm_reply = xterm.process_output(b"\x1b[c\x1b[>c\x1b[?25$p", (0, 0));
        let screen_reply = screen.process_output(b"\x1b[c\x1b[>c\x1b[?25$p", (0, 0));

        assert_eq!(bmux_reply, b"\x1b[?1;2c\x1b[>84;0;0c\x1b[?25;1$y");
        assert_eq!(xterm_reply, b"\x1b[?1;2c\x1b[>0;115;0c\x1b[?25;1$y");
        assert_eq!(
            screen_reply,
            b"\x1b[?64;1;2;6;9;15;18;21;22c\x1b[>83;40003;0c\x1b[?25;1$y"
        );
    }

    #[test]
    fn replies_to_osc_color_queries() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let fg = engine.process_output(b"\x1b]10;?\x1b\\", (0, 0));
        let bg = engine.process_output(b"\x1b]11;?\x1b\\", (0, 0));
        assert_eq!(fg, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
        assert_eq!(bg, b"\x1b]11;rgb:0000/0000/0000\x1b\\");
    }

    #[test]
    fn replies_to_dcs_xtgettcap_queries() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let reply = engine.process_output(b"\x1bP+q5443;636f\x1b\\", (0, 0));
        assert_eq!(reply, b"\x1bP1+r5443=31;636f=323536\x1b\\");
    }

    #[test]
    fn replies_to_dcs_decrqss_query() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let reply = engine.process_output(b"\x1bP$qm\x1b\\", (0, 0));
        assert_eq!(reply, b"\x1bP1$r0m\x1b\\");
    }
}
