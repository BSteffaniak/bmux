//! Terminal UI style primitives.

use bmux_color::Rgb;

use crate::capabilities::TerminalColorDepth;

/// Terminal color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Color {
    /// Backend default foreground/background color.
    #[default]
    Default,
    /// ANSI black.
    Black,
    /// ANSI red.
    Red,
    /// ANSI green.
    Green,
    /// ANSI yellow.
    Yellow,
    /// ANSI blue.
    Blue,
    /// ANSI magenta.
    Magenta,
    /// ANSI cyan.
    Cyan,
    /// ANSI white.
    White,
    /// ANSI bright black / gray.
    BrightBlack,
    /// ANSI bright red.
    BrightRed,
    /// ANSI bright green.
    BrightGreen,
    /// ANSI bright yellow.
    BrightYellow,
    /// ANSI bright blue.
    BrightBlue,
    /// ANSI bright magenta.
    BrightMagenta,
    /// ANSI bright cyan.
    BrightCyan,
    /// ANSI bright white.
    BrightWhite,
    /// 256-color palette index.
    Indexed(u8),
    /// 24-bit RGB color.
    Rgb(u8, u8, u8),
}

/// Text style modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Modifier(u16);

impl Modifier {
    /// No modifiers.
    pub const EMPTY: Self = Self(0);
    /// Bold text.
    pub const BOLD: Self = Self(1 << 0);
    /// Dim text.
    pub const DIM: Self = Self(1 << 1);
    /// Italic text.
    pub const ITALIC: Self = Self(1 << 2);
    /// Underlined text.
    pub const UNDERLINE: Self = Self(1 << 3);
    /// Slow blinking text.
    pub const SLOW_BLINK: Self = Self(1 << 4);
    /// Reversed foreground/background.
    pub const REVERSED: Self = Self(1 << 5);
    /// Hidden text.
    pub const HIDDEN: Self = Self(1 << 6);
    /// Crossed-out text.
    pub const CROSSED_OUT: Self = Self(1 << 7);

    /// Return true when all supplied modifier bits are present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Return a modifier set with the supplied bits added.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Return a modifier set with the supplied bits removed.
    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Return true when no modifier bits are present.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Modifier {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for Modifier {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

/// Cell text style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Style {
    /// Optional foreground color.
    pub fg: Option<Color>,
    /// Optional background color.
    pub bg: Option<Color>,
    /// Text modifiers.
    pub modifiers: Modifier,
}

impl Style {
    /// Create an empty style.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            modifiers: Modifier::EMPTY,
        }
    }

    /// Set the foreground color.
    #[must_use]
    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    /// Set the background color.
    #[must_use]
    pub const fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    /// Add modifiers.
    #[must_use]
    pub const fn add_modifier(mut self, modifier: Modifier) -> Self {
        self.modifiers = self.modifiers.union(modifier);
        self
    }

    /// Remove modifiers.
    #[must_use]
    pub const fn remove_modifier(mut self, modifier: Modifier) -> Self {
        self.modifiers = self.modifiers.difference(modifier);
        self
    }

    /// Merge another style over this one.
    #[must_use]
    pub const fn patch(self, other: Self) -> Self {
        Self {
            fg: if other.fg.is_some() {
                other.fg
            } else {
                self.fg
            },
            bg: if other.bg.is_some() {
                other.bg
            } else {
                self.bg
            },
            modifiers: self.modifiers.union(other.modifiers),
        }
    }

    /// Reduce both colors to `depth`, preserving modifiers.
    ///
    /// See [`Color::quantized`]. On monochrome terminals both colors are
    /// removed, so themes should carry state through modifiers as well.
    #[must_use]
    pub fn quantized(self, depth: TerminalColorDepth) -> Self {
        Self {
            fg: self.fg.and_then(|color| color.quantized(depth)),
            bg: self.bg.and_then(|color| color.quantized(depth)),
            modifiers: self.modifiers,
        }
    }
}

impl Color {
    /// ANSI 16-color variants in palette-index order (0–15).
    pub const ANSI_16: [Self; 16] = [
        Self::Black,
        Self::Red,
        Self::Green,
        Self::Yellow,
        Self::Blue,
        Self::Magenta,
        Self::Cyan,
        Self::White,
        Self::BrightBlack,
        Self::BrightRed,
        Self::BrightGreen,
        Self::BrightYellow,
        Self::BrightBlue,
        Self::BrightMagenta,
        Self::BrightCyan,
        Self::BrightWhite,
    ];

    /// Return the explicit RGB value of [`Color::Rgb`], or `None` for
    /// terminal-owned colors whose displayed value is unknown.
    #[must_use]
    pub const fn rgb(self) -> Option<Rgb> {
        match self {
            Self::Rgb(r, g, b) => Some(Rgb::new(r, g, b)),
            _ => None,
        }
    }

    /// Reduce this color to what a terminal of `depth` can display.
    ///
    /// * `TrueColor` leaves every color unchanged.
    /// * `Ansi256` maps RGB to the nearest fixed cube/grayscale index (16–255).
    /// * `Ansi16` maps RGB and indices 16–255 to the nearest ANSI color using
    ///   xterm default values.
    /// * `Monochrome` returns `None`, removing the color.
    ///
    /// [`Color::Default`] and named ANSI colors are terminal-owned and pass
    /// through unchanged at every depth except `Monochrome`.
    #[must_use]
    pub fn quantized(self, depth: TerminalColorDepth) -> Option<Self> {
        match (depth, self) {
            (TerminalColorDepth::Monochrome, _) => None,
            (TerminalColorDepth::Ansi256, Self::Rgb(r, g, b)) => Some(Self::Indexed(
                bmux_color::ansi::nearest_256(Rgb::new(r, g, b)),
            )),
            (TerminalColorDepth::Ansi16, Self::Rgb(r, g, b)) => Some(Self::ansi_16(
                bmux_color::ansi::nearest_16(Rgb::new(r, g, b)),
            )),
            (TerminalColorDepth::Ansi16, Self::Indexed(index)) if index < 16 => {
                Some(Self::ansi_16(index))
            }
            (TerminalColorDepth::Ansi16, Self::Indexed(index)) => Some(Self::ansi_16(
                bmux_color::ansi::nearest_16(bmux_color::ansi::xterm_256(index)),
            )),
            // True color, terminal defaults, named ANSI colors, and 256-color
            // indices on 256-color terminals are already displayable.
            (_, color) => Some(color),
        }
    }

    const fn ansi_16(index: u8) -> Self {
        Self::ANSI_16[(index & 0x0f) as usize]
    }
}

impl From<Rgb> for Color {
    fn from(color: Rgb) -> Self {
        Self::Rgb(color.r, color.g, color.b)
    }
}

#[cfg(test)]
mod tests {
    use super::{Color, Modifier, Style};
    use crate::capabilities::TerminalColorDepth;

    #[test]
    fn modifier_sets_support_union_and_difference() {
        let modifiers = Modifier::BOLD | Modifier::UNDERLINE;

        assert!(modifiers.contains(Modifier::BOLD));
        assert!(modifiers.contains(Modifier::UNDERLINE));
        assert!(!modifiers.contains(Modifier::ITALIC));
        assert_eq!(modifiers.difference(Modifier::BOLD), Modifier::UNDERLINE);
    }

    #[test]
    fn style_patch_overlays_explicit_fields() {
        let base = Style::new().fg(Color::Green).bg(Color::Black);
        let overlay = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);

        assert_eq!(
            base.patch(overlay),
            Style::new()
                .fg(Color::Red)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn truecolor_keeps_every_color_and_monochrome_removes_all() {
        let style = Style::new()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD);
        assert_eq!(style.quantized(TerminalColorDepth::TrueColor), style);
        assert_eq!(
            style.quantized(TerminalColorDepth::Monochrome),
            Style::new().add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn ansi256_maps_rgb_and_preserves_terminal_owned_colors() {
        let depth = TerminalColorDepth::Ansi256;
        assert_eq!(
            Color::Rgb(0xff, 0, 0).quantized(depth),
            Some(Color::Indexed(196))
        );
        for color in [Color::Default, Color::Cyan, Color::Indexed(42)] {
            assert_eq!(color.quantized(depth), Some(color));
        }
    }

    #[test]
    fn ansi16_maps_rgb_and_extended_indices_to_named_colors() {
        let depth = TerminalColorDepth::Ansi16;
        assert_eq!(
            Color::Rgb(0xf0, 0x10, 0x10).quantized(depth),
            Some(Color::BrightRed)
        );
        assert_eq!(Color::Indexed(4).quantized(depth), Some(Color::Blue));
        assert_eq!(Color::Indexed(232).quantized(depth), Some(Color::Black));
        assert_eq!(Color::Magenta.quantized(depth), Some(Color::Magenta));
        assert_eq!(Color::Default.quantized(depth), Some(Color::Default));
    }

    #[test]
    fn rgb_conversions_round_trip() {
        let rgb = bmux_color::Rgb::new(9, 8, 7);
        assert_eq!(Color::from(rgb).rgb(), Some(rgb));
        assert_eq!(Color::Cyan.rgb(), None);
    }
}
