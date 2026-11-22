//! The dashboard's look: a palette whose colors always mean the same thing, bmux's component
//! theme derived from it, and a stable hue per simulated thread.
//!
//! Meaning: green runs or passed, amber blocks or waits, red failed, violet is a replay (time
//! travel: not live), grey is idle or done, blue informs, the accent marks what you are
//! pointing at. Each personality has its own badge color. Colors are given in full RGB and
//! reduced to the terminal's depth when painted.

use bmux_color::{Oklch, Rgb};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::theme::{ComponentTheme, ComponentThemeSeed};

use crate::telemetry::Tone;

/// The palette's colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The canvas.
    pub background: Rgb,
    /// Panels.
    pub surface: Rgb,
    /// Raised elements: selected rows, cards.
    pub raised: Rgb,
    /// Body text.
    pub text: Rgb,
    /// Secondary text.
    pub muted: Rgb,
    /// Decorations: borders, rules, empty tracks.
    pub faint: Rgb,
    /// What you are pointing at or focused on.
    pub accent: Rgb,
    /// Running, passed.
    pub good: Rgb,
    /// Blocked, waiting, unusual.
    pub warn: Rgb,
    /// Failed, violated.
    pub bad: Rgb,
    /// A replay: not live.
    pub replay: Rgb,
    /// Informational.
    pub info: Rgb,
    /// Whether the palette is dark.
    pub dark: bool,
}

impl Palette {
    /// The default, dark palette.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            background: Rgb::new(0x10, 0x13, 0x1a),
            surface: Rgb::new(0x16, 0x1a, 0x23),
            raised: Rgb::new(0x22, 0x28, 0x36),
            text: Rgb::new(0xd8, 0xde, 0xe9),
            muted: Rgb::new(0x8a, 0x93, 0xa6),
            faint: Rgb::new(0x3b, 0x42, 0x52),
            accent: Rgb::new(0x7a, 0xa2, 0xf7),
            good: Rgb::new(0x9e, 0xce, 0x6a),
            warn: Rgb::new(0xe0, 0xaf, 0x68),
            bad: Rgb::new(0xf7, 0x76, 0x8e),
            replay: Rgb::new(0xbb, 0x9a, 0xf7),
            info: Rgb::new(0x7d, 0xcf, 0xff),
            dark: true,
        }
    }

    /// A light palette.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            background: Rgb::new(0xf7, 0xf7, 0xf4),
            surface: Rgb::new(0xee, 0xee, 0xea),
            raised: Rgb::new(0xdd, 0xe1, 0xea),
            text: Rgb::new(0x24, 0x28, 0x33),
            muted: Rgb::new(0x5f, 0x66, 0x75),
            faint: Rgb::new(0xb8, 0xbd, 0xc8),
            accent: Rgb::new(0x2e, 0x5c, 0xc8),
            good: Rgb::new(0x38, 0x7a, 0x1f),
            warn: Rgb::new(0x9a, 0x62, 0x00),
            bad: Rgb::new(0xc0, 0x23, 0x45),
            replay: Rgb::new(0x7a, 0x3c, 0xd0),
            info: Rgb::new(0x00, 0x6f, 0xa6),
            dark: false,
        }
    }
}

/// The palette and everything derived from it.
#[derive(Debug, Clone)]
pub struct Theme {
    /// The palette.
    pub palette: Palette,
    /// bmux's component theme, derived from the palette.
    pub components: ComponentTheme,
}

const fn color(rgb: Rgb) -> Color {
    Color::Rgb(rgb.r, rgb.g, rgb.b)
}

impl Theme {
    /// The theme for `palette`.
    #[must_use]
    pub fn new(palette: Palette) -> Self {
        let components = ComponentTheme::derive(ComponentThemeSeed {
            background: palette.background,
            foreground: palette.text,
            accent: palette.accent,
            info: palette.info,
            success: palette.good,
            warning: palette.warn,
            error: palette.bad,
        });
        Self {
            palette,
            components,
        }
    }

    /// The dark theme.
    #[must_use]
    pub fn dark() -> Self {
        Self::new(Palette::dark())
    }

    /// The light theme.
    #[must_use]
    pub fn light() -> Self {
        Self::new(Palette::light())
    }

    /// The other theme.
    #[must_use]
    pub fn toggled(&self) -> Self {
        if self.palette.dark {
            Self::light()
        } else {
            Self::dark()
        }
    }

    /// Text in `rgb`.
    #[must_use]
    pub const fn fg(&self, rgb: Rgb) -> Style {
        Style::new().fg(color(rgb))
    }

    /// The canvas.
    #[must_use]
    pub const fn canvas(&self) -> Style {
        Style::new()
            .fg(color(self.palette.text))
            .bg(color(self.palette.background))
    }

    /// Body text.
    #[must_use]
    pub const fn text(&self) -> Style {
        self.fg(self.palette.text)
    }

    /// Secondary text.
    #[must_use]
    pub const fn muted(&self) -> Style {
        self.fg(self.palette.muted)
    }

    /// Decorations.
    #[must_use]
    pub const fn faint(&self) -> Style {
        self.fg(self.palette.faint)
    }

    /// Emphasis.
    #[must_use]
    pub const fn strong(&self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }

    /// The accent.
    #[must_use]
    pub const fn accent(&self) -> Style {
        self.fg(self.palette.accent)
    }

    /// Running, passed.
    #[must_use]
    pub const fn good(&self) -> Style {
        self.fg(self.palette.good)
    }

    /// Waiting, unusual.
    #[must_use]
    pub const fn warn(&self) -> Style {
        self.fg(self.palette.warn)
    }

    /// Failed.
    #[must_use]
    pub const fn bad(&self) -> Style {
        self.fg(self.palette.bad)
    }

    /// A replay.
    #[must_use]
    pub const fn replay(&self) -> Style {
        self.fg(self.palette.replay)
    }

    /// Informational.
    #[must_use]
    pub const fn info(&self) -> Style {
        self.fg(self.palette.info)
    }

    /// A raised row (selected or hovered).
    #[must_use]
    pub const fn raised(&self) -> Style {
        Style::new()
            .bg(color(self.palette.raised))
            .fg(color(self.palette.text))
    }

    /// A row the pointer is over.
    #[must_use]
    pub fn hover(&self) -> Style {
        Style::new().bg(color(bmux_color::mix(
            self.palette.surface,
            self.palette.raised,
            0.6,
        )))
    }

    /// Panels.
    #[must_use]
    pub const fn surface(&self) -> Style {
        Style::new()
            .bg(color(self.palette.surface))
            .fg(color(self.palette.text))
    }

    /// The style an event of `tone` reads in.
    #[must_use]
    pub const fn tone(&self, tone: Tone) -> Style {
        match tone {
            Tone::Neutral => self.text(),
            Tone::Info => self.info(),
            Tone::Good => self.good(),
            Tone::Warn => self.warn(),
            Tone::Bad => self.bad(),
        }
    }

    /// The style a scheduler thread state reads in (`running`, `blocked ...`, `finished`).
    #[must_use]
    pub fn thread_state(&self, state: &str) -> Style {
        if state.starts_with("running") {
            self.good()
        } else if state.starts_with("blocked") || state.starts_with("sleep") {
            self.warn()
        } else if state.starts_with("finished") || state.starts_with("exited") {
            self.muted()
        } else if state.contains("panic") {
            self.bad()
        } else {
            self.info()
        }
    }

    /// A glyph for a scheduler thread state, so the state reads without color too.
    #[must_use]
    pub fn thread_glyph(state: &str) -> &'static str {
        if state.starts_with("running") {
            "▶"
        } else if state.starts_with("blocked") {
            "⏸"
        } else if state.starts_with("sleep") {
            "◷"
        } else if state.starts_with("finished") || state.starts_with("exited") {
            "·"
        } else if state.contains("panic") {
            "✖"
        } else {
            "○"
        }
    }

    /// The color of thread `index` (of a run's threads), the same everywhere it appears:
    /// hues spread by the golden angle, at a lightness that reads on the canvas.
    #[must_use]
    pub fn thread(&self, index: usize) -> Rgb {
        #[allow(clippy::cast_precision_loss)] // small indices
        let hue = (index as f64 * 137.507_764).rem_euclid(360.0);
        let lightness = if self.palette.dark { 0.76 } else { 0.52 };
        let rgb = Oklch {
            l: lightness,
            c: 0.12,
            h: hue,
        }
        .to_rgb();
        bmux_color::ensure_contrast(rgb, self.palette.background, 3.0)
    }

    /// The badge color of a personality (`linux`, `macos`, `windows`), or of anything else.
    #[must_use]
    pub const fn personality(&self, name: &str) -> Rgb {
        match name.as_bytes() {
            b"linux" => Rgb::new(0xf2, 0xa2, 0x55),
            b"macos" => Rgb::new(0xc8, 0xcf, 0xdc),
            b"windows" => Rgb::new(0x4e, 0xb0, 0xf2),
            _ => self.palette.muted,
        }
    }

    /// A color between `from` and `to` (`t` from 0 to 1), for heat.
    #[must_use]
    pub fn blend(from: Rgb, to: Rgb, t: f64) -> Rgb {
        bmux_color::mix(from, to, t.clamp(0.0, 1.0))
    }

    /// A color from the theme's palette as a terminal color.
    #[must_use]
    pub const fn color(rgb: Rgb) -> Color {
        color(rgb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_colors_are_distinct_and_readable() {
        for theme in [Theme::dark(), Theme::light()] {
            let colors: Vec<Rgb> = (0..8).map(|i| theme.thread(i)).collect();
            for (i, a) in colors.iter().enumerate() {
                assert!(a.contrast_ratio(theme.palette.background) >= 2.9);
                for b in &colors[i + 1..] {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn states_read_without_color() {
        assert_eq!(Theme::thread_glyph("running"), "▶");
        assert_eq!(Theme::thread_glyph("blocked on channel"), "⏸");
        assert_eq!(Theme::thread_glyph("finished"), "·");
    }
}
