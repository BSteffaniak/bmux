//! Bounded terminal presentation capability detection.
//!
//! These values describe renderer capabilities only. They do not select an
//! application theme or alter product behavior.

/// Best-effort terminal background appearance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TerminalBackground {
    /// The terminal did not expose a trustworthy background hint.
    #[default]
    Unknown,
    /// The terminal background is expected to be dark.
    Dark,
    /// The terminal background is expected to be light.
    Light,
}

/// Best-effort terminal color depth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TerminalColorDepth {
    /// Color presentation was explicitly disabled.
    Monochrome,
    /// ANSI 16-color presentation.
    #[default]
    Ansi16,
    /// Indexed 256-color presentation.
    Ansi256,
    /// 24-bit RGB presentation.
    TrueColor,
}

/// Renderer-neutral facts detected from the terminal process environment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalCapabilities {
    /// Best-effort background appearance.
    pub background: TerminalBackground,
    /// Best-effort supported color depth.
    pub color_depth: TerminalColorDepth,
}

impl TerminalCapabilities {
    /// Detect capabilities from the current process environment.
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_with(|name| std::env::var(name).ok())
    }

    /// Detect capabilities from an explicit environment lookup.
    ///
    /// This keeps policy deterministic and testable while allowing native
    /// frontends to supply a captured environment instead of global state.
    #[must_use]
    pub fn detect_with(mut value: impl FnMut(&str) -> Option<String>) -> Self {
        let no_color = value("NO_COLOR").is_some();
        let color_term = value("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        let term = value("TERM").unwrap_or_default().to_ascii_lowercase();
        let color_depth = if no_color || term == "dumb" {
            TerminalColorDepth::Monochrome
        } else if matches!(color_term.as_str(), "truecolor" | "24bit") {
            TerminalColorDepth::TrueColor
        } else if term.contains("256color") {
            TerminalColorDepth::Ansi256
        } else {
            TerminalColorDepth::Ansi16
        };
        let background = value("COLORFGBG")
            .as_deref()
            .and_then(parse_colorfgbg_background)
            .unwrap_or_default();
        Self {
            background,
            color_depth,
        }
    }
}

impl TerminalCapabilities {
    /// Whether the terminal is known to implement left/right margin mode
    /// (DEC private mode 69 with DECSLRM), detected from the current process
    /// environment. See [`Self::left_right_margins_with`].
    #[must_use]
    pub fn left_right_margins() -> bool {
        Self::left_right_margins_with(|name| std::env::var(name).ok())
    }

    /// Whether the terminal is known to implement left/right margin mode,
    /// detected from an explicit environment lookup.
    ///
    /// Terminals that do not implement the mode misread its sequences, so
    /// this is a conservative allowlist: true only for terminals identified
    /// directly (iTerm2 and Ghostty through `TERM_PROGRAM`, Ghostty through
    /// `TERM`, xterm through `XTERM_VERSION`) and never inside a multiplexer
    /// (`TMUX`, `STY`, or a `screen`/`tmux` `TERM`), which presents its own
    /// screen to the application.
    #[must_use]
    pub fn left_right_margins_with(mut value: impl FnMut(&str) -> Option<String>) -> bool {
        let term = value("TERM").unwrap_or_default().to_ascii_lowercase();
        if value("TMUX").is_some()
            || value("STY").is_some()
            || term.starts_with("screen")
            || term.starts_with("tmux")
        {
            return false;
        }
        let program = value("TERM_PROGRAM").unwrap_or_default();
        matches!(program.as_str(), "iTerm.app" | "ghostty")
            || term == "xterm-ghostty"
            || value("XTERM_VERSION").is_some()
    }
}

fn parse_colorfgbg_background(value: &str) -> Option<TerminalBackground> {
    let index = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    Some(if index <= 6 || index == 8 {
        TerminalBackground::Dark
    } else {
        TerminalBackground::Light
    })
}

/// Number of ANSI palette entries requested by [`TerminalColorReport::QUERY`].
pub const REPORTED_PALETTE_LEN: usize = 16;

/// Terminal-reported default colors and base palette.
///
/// Terminals answer OSC 10 (foreground), OSC 11 (background), and OSC 4
/// (palette) queries with their actual RGB values. Applications write
/// [`Self::QUERY`] once and feed opaque input frames to [`Self::observe`];
/// terminals that do not support a query simply never answer it, so every
/// field stays optional and nothing blocks waiting for a reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalColorReport {
    /// Default foreground color.
    pub foreground: Option<bmux_color::Rgb>,
    /// Default background color.
    pub background: Option<bmux_color::Rgb>,
    /// ANSI palette entries 0–15.
    pub palette: [Option<bmux_color::Rgb>; REPORTED_PALETTE_LEN],
}

impl TerminalColorReport {
    /// Query for the default foreground, background, and palette entries 0–15.
    pub const QUERY: &'static [u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]4;0;?;1;?;2;?;3;?;4;?;5;?;6;?;7;?;8;?;9;?;10;?;11;?;12;?;13;?;14;?;15;?\x1b\\";

    /// Record a color reply contained in one complete OSC frame.
    ///
    /// Returns `true` when the frame was a recognized color report (and should
    /// therefore not be treated as user input). Malformed or unrelated frames
    /// return `false` and leave the report unchanged.
    pub fn observe(&mut self, frame: &[u8]) -> bool {
        let Some(body) = osc_body(frame) else {
            return false;
        };
        let Ok(body) = std::str::from_utf8(body) else {
            return false;
        };
        let mut fields = body.split(';');
        match fields.next() {
            Some("10") => fields
                .next()
                .and_then(parse_x11_rgb)
                .map(|color| self.foreground = Some(color))
                .is_some(),
            Some("11") => fields
                .next()
                .and_then(parse_x11_rgb)
                .map(|color| self.background = Some(color))
                .is_some(),
            Some("4") => {
                let mut recognized = false;
                while let (Some(index), Some(value)) = (fields.next(), fields.next()) {
                    let (Ok(index), Some(color)) = (index.parse::<usize>(), parse_x11_rgb(value))
                    else {
                        continue;
                    };
                    if let Some(slot) = self.palette.get_mut(index) {
                        *slot = Some(color);
                        recognized = true;
                    }
                }
                recognized
            }
            _ => false,
        }
    }

    /// Background appearance implied by the reported background color.
    #[must_use]
    pub fn background_appearance(&self) -> TerminalBackground {
        self.background
            .map_or(TerminalBackground::Unknown, |background| {
                if background.contrast_ratio(bmux_color::Rgb::WHITE)
                    >= background.contrast_ratio(bmux_color::Rgb::BLACK)
                {
                    TerminalBackground::Dark
                } else {
                    TerminalBackground::Light
                }
            })
    }

    /// Return the reported palette as a complete array when every entry is known.
    #[must_use]
    pub fn complete_palette(&self) -> Option<[bmux_color::Rgb; REPORTED_PALETTE_LEN]> {
        let mut palette = [bmux_color::Rgb::BLACK; REPORTED_PALETTE_LEN];
        for (slot, reported) in palette.iter_mut().zip(self.palette) {
            *slot = reported?;
        }
        Some(palette)
    }
}

/// Strip the OSC introducer and BEL/ST terminator from one frame.
fn osc_body(frame: &[u8]) -> Option<&[u8]> {
    let body = frame.strip_prefix(b"\x1b]")?;
    body.strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(b"\x07"))
}

/// Parse X11 `rgb:R/G/B` (1–4 hex digits per channel) or `#rrggbb` color specs.
fn parse_x11_rgb(value: &str) -> Option<bmux_color::Rgb> {
    if let Some(hex) = value.strip_prefix('#') {
        return (hex.len() == 6).then(|| bmux_color::Rgb::from_hex(hex).ok())?;
    }
    let channels = value.strip_prefix("rgb:")?;
    let mut parts = channels.split('/');
    let mut parse = || -> Option<u8> {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 4 {
            return None;
        }
        let raw = u32::from_str_radix(part, 16).ok()?;
        #[allow(clippy::cast_possible_truncation)] // part.len() is in 1..=4.
        let max = (1_u32 << (4 * part.len() as u32)) - 1;
        u8::try_from((raw * 255 + max / 2) / max).ok()
    };
    let color = bmux_color::Rgb::new(parse()?, parse()?, parse()?);
    parts.next().is_none().then_some(color)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn detect(values: &[(&str, &str)]) -> TerminalCapabilities {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();
        TerminalCapabilities::detect_with(|name| values.get(name).cloned())
    }

    #[test]
    fn detects_background_and_truecolor_independently() {
        assert_eq!(
            detect(&[("COLORFGBG", "15;0"), ("COLORTERM", "truecolor")]),
            TerminalCapabilities {
                background: TerminalBackground::Dark,
                color_depth: TerminalColorDepth::TrueColor,
            }
        );
        assert_eq!(
            detect(&[("COLORFGBG", "0;15"), ("TERM", "xterm-256color")]),
            TerminalCapabilities {
                background: TerminalBackground::Light,
                color_depth: TerminalColorDepth::Ansi256,
            }
        );
    }

    #[test]
    fn unknown_background_and_no_color_are_conservative() {
        assert_eq!(
            detect(&[("NO_COLOR", "1"), ("COLORFGBG", "unknown")]),
            TerminalCapabilities {
                background: TerminalBackground::Unknown,
                color_depth: TerminalColorDepth::Monochrome,
            }
        );
    }

    #[test]
    fn left_right_margins_are_detected_only_for_known_terminals_outside_multiplexers() {
        let margins = |values: &[(&str, &str)]| {
            let values = values
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>();
            TerminalCapabilities::left_right_margins_with(|name| values.get(name).cloned())
        };
        assert!(margins(&[("TERM_PROGRAM", "iTerm.app")]));
        assert!(margins(&[("TERM", "xterm-ghostty")]));
        assert!(margins(&[
            ("TERM", "xterm-256color"),
            ("XTERM_VERSION", "XTerm(390)")
        ]));
        assert!(!margins(&[("TERM_PROGRAM", "Apple_Terminal")]));
        assert!(!margins(&[("TERM", "xterm-256color")]));
        assert!(!margins(&[
            ("TERM_PROGRAM", "iTerm.app"),
            ("TMUX", "/tmp/tmux,1,0")
        ]));
        assert!(!margins(&[
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM", "screen-256color")
        ]));
    }

    #[test]
    fn color_report_parses_st_and_bel_replies_with_any_digit_width() {
        let mut report = TerminalColorReport::default();
        assert!(report.observe(b"\x1b]11;rgb:1e1e/2e2e/3e3e\x1b\\"));
        assert!(report.observe(b"\x1b]10;rgb:f/e/d\x07"));
        assert!(report.observe(b"\x1b]4;1;rgb:cd/00/00;9;#ff0000\x1b\\"));
        assert_eq!(
            report.background,
            Some(bmux_color::Rgb::new(0x1e, 0x2e, 0x3e))
        );
        assert_eq!(
            report.foreground,
            Some(bmux_color::Rgb::new(0xff, 0xee, 0xdd))
        );
        assert_eq!(report.palette[1], Some(bmux_color::Rgb::new(0xcd, 0, 0)));
        assert_eq!(report.palette[9], Some(bmux_color::Rgb::new(0xff, 0, 0)));
        assert_eq!(report.background_appearance(), TerminalBackground::Dark);
        assert_eq!(report.complete_palette(), None);
    }

    #[test]
    fn color_report_ignores_unrelated_and_malformed_frames() {
        let mut report = TerminalColorReport::default();
        for frame in [
            b"\x1b]11;?\x1b\\".as_slice(),
            b"\x1b]11;rgb:zz/00/00\x1b\\",
            b"\x1b]11;rgb:00/00\x1b\\",
            b"\x1b]11;rgb:00000/0/0\x1b\\",
            b"\x1b]4;99;rgb:0/0/0\x1b\\",
            b"\x1b]52;c;aGk=\x07",
            b"\x1b[?1;2c",
            b"\x1b]11;rgb:0/0/0",
        ] {
            assert!(!report.observe(frame), "{frame:?}");
        }
        assert_eq!(report, TerminalColorReport::default());
    }

    #[test]
    fn light_background_and_complete_palette_are_reported() {
        let mut report = TerminalColorReport::default();
        report.observe(b"\x1b]11;rgb:ffff/ffff/f0f0\x1b\\");
        assert_eq!(report.background_appearance(), TerminalBackground::Light);
        for index in 0..REPORTED_PALETTE_LEN {
            let frame = format!("\x1b]4;{index};rgb:{index:x}/0/0\x07");
            assert!(report.observe(frame.as_bytes()));
        }
        assert_eq!(
            report.complete_palette().map(|palette| palette[15].r),
            Some(255)
        );
    }

    #[test]
    fn query_requests_each_reported_color() {
        let query = std::str::from_utf8(TerminalColorReport::QUERY).unwrap();
        assert!(query.contains("]10;?") && query.contains("]11;?"));
        assert!(query.contains(";15;?"));
    }
}
