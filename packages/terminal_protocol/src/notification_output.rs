//! The bell and desktop notifications on a host terminal.
//!
//! The bell is `BEL`, which every terminal takes. Desktop notifications use
//! one of two private sequences: `OSC 777 ; notify ; title ; body` (Ghostty,
//! WezTerm, foot, urxvt and VTE-based terminals) or `OSC 9 ; text` (iTerm2,
//! kitty and most others that show notifications). Text from programs and
//! agents is untrusted, so control characters never reach the terminal.

/// How the host terminal shows desktop notifications.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotificationCapability {
    /// `OSC 777 ; notify ; title ; body`.
    Osc777,
    /// `OSC 9 ; text`.
    #[default]
    Osc9,
    /// Notifications are not shown; the bell still rings.
    Unsupported,
}

/// Environment variable that overrides notification capability detection:
/// `osc777`, `osc9`, or `off`.
pub const NOTIFICATIONS_ENV: &str = "BMUX_NOTIFICATIONS";

/// The most characters of a title or body that reach the terminal.
const MAX_TEXT_CHARS: usize = 200;

impl NotificationCapability {
    /// From the terminal's `TERM` and `TERM_PROGRAM`, whether it is VTE-based
    /// (`VTE_VERSION` is set), and an explicit override.
    #[must_use]
    pub fn from_environment(
        term: &str,
        program: &str,
        vte: bool,
        override_value: Option<&str>,
    ) -> Self {
        if let Some(value) = override_value {
            return match value {
                "osc777" => Self::Osc777,
                "osc9" => Self::Osc9,
                _ => Self::Unsupported,
            };
        }
        let term = term.to_ascii_lowercase();
        let program = program.to_ascii_lowercase();
        if term == "dumb" {
            return Self::Unsupported;
        }
        if matches!(program.as_str(), "ghostty" | "wezterm" | "foot")
            || term.contains("ghostty")
            || term.starts_with("foot")
            || term.starts_with("rxvt")
            || vte
        {
            return Self::Osc777;
        }
        Self::Osc9
    }

    /// The running terminal's capability. Inside tmux, notifications would
    /// reach tmux rather than the terminal, so only the bell rings.
    #[must_use]
    pub fn detect() -> Self {
        let override_value = std::env::var(NOTIFICATIONS_ENV).ok();
        if override_value.is_none()
            && std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
        {
            return Self::Unsupported;
        }
        Self::from_environment(
            &std::env::var("TERM").unwrap_or_default(),
            &std::env::var("TERM_PROGRAM").unwrap_or_default(),
            std::env::var_os("VTE_VERSION").is_some(),
            override_value.as_deref(),
        )
    }
}

/// The bytes that ring the bell (when `bell`) and show each of
/// `notifications` (title, body) with `capability`.
#[must_use]
pub fn encode_alerts(
    bell: bool,
    notifications: &[(&str, &str)],
    capability: NotificationCapability,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    if bell {
        bytes.push(0x07);
    }
    for (title, body) in notifications {
        let title = sanitize(title);
        let body = sanitize(body);
        match capability {
            NotificationCapability::Osc777 => {
                // `;` separates the title from the body.
                let title = title.replace(';', ",");
                bytes.extend_from_slice(format!("\x1b]777;notify;{title};{body}\x1b\\").as_bytes());
            }
            NotificationCapability::Osc9 => {
                let text = match (title.is_empty(), body.is_empty()) {
                    (false, false) => format!("{title}: {body}"),
                    (false, true) => title,
                    _ => body,
                };
                // `OSC 9 ; 4 ; …` is a progress report, not a notification.
                let text = if text.starts_with(|c: char| c.is_ascii_digit()) {
                    format!(" {text}")
                } else {
                    text
                };
                bytes.extend_from_slice(format!("\x1b]9;{text}\x1b\\").as_bytes());
            }
            NotificationCapability::Unsupported => {}
        }
    }
    bytes
}

/// `text` without control characters (C0, DEL, C1), whitespace runs
/// collapsed, at most [`MAX_TEXT_CHARS`] characters.
fn sanitize(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in text.chars() {
        let control = ch.is_control();
        if ch.is_whitespace() || control {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    match out.char_indices().nth(MAX_TEXT_CHARS) {
        Some((index, _)) => {
            out.truncate(index);
            out.push('…');
            out
        }
        None => out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_get_the_notification_sequence_they_show() {
        let detect = |term: &str, program: &str, vte: bool| {
            NotificationCapability::from_environment(term, program, vte, None)
        };
        assert_eq!(
            detect("xterm-ghostty", "ghostty", false),
            NotificationCapability::Osc777
        );
        assert_eq!(
            detect("xterm-256color", "WezTerm", false),
            NotificationCapability::Osc777
        );
        assert_eq!(detect("foot", "", false), NotificationCapability::Osc777);
        assert_eq!(
            detect("rxvt-unicode", "", false),
            NotificationCapability::Osc777
        );
        assert_eq!(
            detect("xterm-256color", "", true),
            NotificationCapability::Osc777
        );
        assert_eq!(
            detect("xterm-256color", "iTerm.app", false),
            NotificationCapability::Osc9
        );
        assert_eq!(
            detect("xterm-kitty", "", false),
            NotificationCapability::Osc9
        );
        assert_eq!(
            detect("dumb", "", false),
            NotificationCapability::Unsupported
        );
        assert_eq!(
            NotificationCapability::from_environment(
                "xterm-ghostty",
                "ghostty",
                false,
                Some("off")
            ),
            NotificationCapability::Unsupported
        );
        assert_eq!(
            NotificationCapability::from_environment("dumb", "", false, Some("osc9")),
            NotificationCapability::Osc9
        );
    }

    #[test]
    fn alerts_encode_the_bell_and_each_notification() {
        let notification = [("bmux", "claude in work/api needs input: proceed?")];
        assert_eq!(
            encode_alerts(true, &[], NotificationCapability::Osc9),
            b"\x07"
        );
        assert_eq!(
            encode_alerts(true, &notification, NotificationCapability::Osc777),
            b"\x07\x1b]777;notify;bmux;claude in work/api needs input: proceed?\x1b\\"
        );
        assert_eq!(
            encode_alerts(false, &notification, NotificationCapability::Osc9),
            b"\x1b]9;bmux: claude in work/api needs input: proceed?\x1b\\"
        );
        assert_eq!(
            encode_alerts(true, &notification, NotificationCapability::Unsupported),
            b"\x07"
        );
    }

    /// Untrusted text cannot end the sequence or inject its own: control
    /// characters go, the 777 title keeps no `;`, and OSC 9 text is never
    /// read as a progress report.
    #[test]
    fn untrusted_text_cannot_escape_the_sequence() {
        let bytes = encode_alerts(
            false,
            &[("a;b\x1b]0;x\x07", "line\n\tnext\x1b\\\u{9b}31m\u{7}")],
            NotificationCapability::Osc777,
        );
        // Control characters break words; none survive.
        assert_eq!(bytes, b"\x1b]777;notify;a,b ]0,x;line next \\ 31m\x1b\\");
        let bytes = encode_alerts(false, &[("", "4;3")], NotificationCapability::Osc9);
        assert_eq!(bytes, b"\x1b]9; 4;3\x1b\\");
        let long = "x".repeat(500);
        let bytes = encode_alerts(false, &[("", &long)], NotificationCapability::Osc9);
        assert_eq!(
            String::from_utf8(bytes)
                .unwrap()
                .chars()
                .filter(|c| *c == 'x')
                .count(),
            200
        );
    }
}
