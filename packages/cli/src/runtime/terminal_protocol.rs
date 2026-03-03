#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Ground,
    Esc,
    Csi,
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
    profile: ProtocolProfile,
}

impl TerminalProtocolEngine {
    pub(super) fn new(profile: ProtocolProfile) -> Self {
        Self {
            state: ParseState::Ground,
            csi_buffer: Vec::new(),
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
        "csi_dec_mode_report",
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
        _ if sequence.starts_with(b"?") && sequence.ends_with(b"$p") => {
            dec_mode_report_response(sequence, profile)
        }
        _ => None,
    }
}

fn dec_mode_report_response(sequence: &[u8], profile: ProtocolProfile) -> Option<Vec<u8>> {
    let mode_bytes = &sequence[1..sequence.len().saturating_sub(2)];
    let mode = std::str::from_utf8(mode_bytes).ok()?.parse::<u16>().ok()?;
    let status = dec_mode_status(profile, mode);
    Some(format!("\x1b[?{mode};{status}$y").into_bytes())
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
}
