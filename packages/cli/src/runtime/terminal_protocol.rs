#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Da1State {
    Ground,
    Esc,
    Csi,
    CsiZero,
}

#[derive(Debug, Default)]
pub(super) struct TerminalProtocolEngine {
    da1_state: Option<Da1State>,
}

impl TerminalProtocolEngine {
    pub(super) fn process_output(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        let mut state = self.da1_state.unwrap_or(Da1State::Ground);

        for byte in bytes {
            state = match state {
                Da1State::Ground => {
                    if *byte == 0x1b {
                        Da1State::Esc
                    } else {
                        Da1State::Ground
                    }
                }
                Da1State::Esc => {
                    if *byte == b'[' {
                        Da1State::Csi
                    } else if *byte == 0x1b {
                        Da1State::Esc
                    } else {
                        Da1State::Ground
                    }
                }
                Da1State::Csi => {
                    if *byte == b'c' {
                        replies.extend_from_slice(primary_da_response());
                        Da1State::Ground
                    } else if *byte == b'0' {
                        Da1State::CsiZero
                    } else if *byte == 0x1b {
                        Da1State::Esc
                    } else {
                        Da1State::Ground
                    }
                }
                Da1State::CsiZero => {
                    if *byte == b'c' {
                        replies.extend_from_slice(primary_da_response());
                        Da1State::Ground
                    } else if *byte == 0x1b {
                        Da1State::Esc
                    } else {
                        Da1State::Ground
                    }
                }
            };
        }

        self.da1_state = Some(state);
        replies
    }
}

pub(super) fn supported_query_names() -> &'static [&'static str] {
    &["csi_primary_da"]
}

fn primary_da_response() -> &'static [u8] {
    b"\x1b[?1;2c"
}

#[cfg(test)]
mod tests {
    use super::TerminalProtocolEngine;

    #[test]
    fn replies_to_primary_da_query() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[c");
        assert_eq!(reply, b"\x1b[?1;2c");
    }

    #[test]
    fn replies_to_primary_da_with_zero_param() {
        let mut engine = TerminalProtocolEngine::default();
        let reply = engine.process_output(b"\x1b[0c");
        assert_eq!(reply, b"\x1b[?1;2c");
    }

    #[test]
    fn handles_split_sequences_across_chunks() {
        let mut engine = TerminalProtocolEngine::default();
        assert!(engine.process_output(b"\x1b[").is_empty());
        let reply = engine.process_output(b"c");
        assert_eq!(reply, b"\x1b[?1;2c");
    }
}
