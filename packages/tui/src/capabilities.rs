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

fn parse_colorfgbg_background(value: &str) -> Option<TerminalBackground> {
    let index = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
    Some(if index <= 6 || index == 8 {
        TerminalBackground::Dark
    } else {
        TerminalBackground::Light
    })
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
}
