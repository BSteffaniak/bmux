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

/// Timing curve for animations: maps linear progress to eased progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Easing {
    /// Constant speed.
    #[default]
    Linear,
    /// Starts slow, ends fast (quadratic).
    EaseIn,
    /// Starts fast, ends slow (cubic).
    EaseOut,
    /// Slow at both ends (cubic).
    EaseInOut,
}

impl Easing {
    /// Eased value of `progress`, clamped to 0..=1 first; 0 maps to 0 and 1 to 1.
    #[must_use]
    pub fn apply(self, progress: f64) -> f64 {
        let t = if progress.is_nan() {
            0.0
        } else {
            progress.clamp(0.0, 1.0)
        };
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t,
            Self::EaseOut => 1.0 - (1.0 - t).powi(3),
            Self::EaseInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0f64).mul_add(t, 2.0).powi(3) / 2.0
                }
            }
        }
    }

    /// Eased progress after `elapsed` of an animation lasting `duration`: 0 at the start,
    /// 1 once `duration` has passed. A zero `duration` is already complete.
    #[must_use]
    pub fn progress(self, elapsed: std::time::Duration, duration: std::time::Duration) -> f64 {
        if duration.is_zero() {
            return 1.0;
        }
        self.apply(elapsed.as_secs_f64() / duration.as_secs_f64())
    }
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

    /// Approximate RGB value of this color: [`Color::Rgb`] as is, and ANSI and 256-color
    /// palette entries by their xterm defaults (terminals may be configured differently).
    /// [`Color::Default`] has no known value and returns `None`.
    #[must_use]
    pub fn approximate_rgb(self) -> Option<Rgb> {
        match self {
            Self::Default => None,
            Self::Rgb(r, g, b) => Some(Rgb::new(r, g, b)),
            Self::Indexed(index) => Some(bmux_color::ansi::xterm_256(index)),
            named => Self::ANSI_16
                .iter()
                .position(|ansi| *ansi == named)
                .and_then(|index| u8::try_from(index).ok())
                .map(bmux_color::ansi::xterm_256),
        }
    }

    /// Blend from this color toward `target` (in OKLab, see [`bmux_color::mix`]): `amount`
    /// 0 is this color, 1 is `target`, and values outside that range are clamped.
    ///
    /// The result is [`Color::Rgb`], computed from [`Color::approximate_rgb`] of both
    /// colors. Returns `None` when either has no known value ([`Color::Default`]), so
    /// callers can choose their own fallback, such as switching colors without a blend.
    #[must_use]
    pub fn mix(self, target: Self, amount: f64) -> Option<Self> {
        let (from, to) = (self.approximate_rgb()?, target.approximate_rgb()?);
        let amount = if amount.is_nan() { 0.0 } else { amount };
        Some(bmux_color::mix(to, from, amount).into())
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
    use super::{Color, Easing, Modifier, Style};
    use crate::capabilities::TerminalColorDepth;
    use std::time::Duration;

    #[test]
    fn colors_blend_between_endpoints_and_clamp_the_amount() {
        let from = Color::Rgb(0, 100, 255);
        let to = Color::Rgb(200, 0, 55);
        assert_eq!(from.mix(to, 0.0), Some(from));
        assert_eq!(from.mix(to, 1.0), Some(to));
        assert_eq!(from.mix(to, -3.0), Some(from));
        assert_eq!(from.mix(to, 7.0), Some(to));
        assert_eq!(from.mix(to, f64::NAN), Some(from));
        // Halfway lies strictly between the endpoints.
        let Some(Color::Rgb(r, g, b)) = from.mix(to, 0.5) else {
            panic!("blends are RGB");
        };
        assert!(
            r > 0 && r < 200 && g < 100 && b > 55 && b < 255,
            "{r} {g} {b}"
        );
        // Palette colors blend through their xterm defaults.
        assert_eq!(
            Color::Black.mix(Color::BrightWhite, 1.0),
            Some(Color::Rgb(255, 255, 255))
        );
        assert_eq!(
            Color::Indexed(196).mix(Color::Black, 0.0),
            Some(Color::Rgb(255, 0, 0))
        );
        assert_eq!(
            Color::Indexed(9).approximate_rgb(),
            Color::BrightRed.approximate_rgb()
        );
        assert_eq!(
            Color::Indexed(232).approximate_rgb(),
            Some(bmux_color::Rgb::new(8, 8, 8))
        );
        // The terminal's default color has no known value.
        assert_eq!(Color::Default.mix(to, 0.5), None);
        assert_eq!(from.mix(Color::Default, 0.5), None);
    }

    #[test]
    fn easing_curves_keep_their_endpoints_and_shapes() {
        for easing in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            assert!(easing.apply(0.0).abs() < f64::EPSILON, "{easing:?}");
            assert!((easing.apply(1.0) - 1.0).abs() < f64::EPSILON, "{easing:?}");
            assert!(easing.apply(-1.0).abs() < f64::EPSILON, "{easing:?}");
            assert!((easing.apply(2.0) - 1.0).abs() < f64::EPSILON, "{easing:?}");
        }
        assert!(Easing::EaseIn.apply(0.5) < 0.5);
        assert!(Easing::EaseOut.apply(0.5) > 0.5);
        assert!((Easing::EaseInOut.apply(0.5) - 0.5).abs() < 1e-6);
        assert!((Easing::Linear.apply(0.25) - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn easing_progress_follows_elapsed_time() {
        let second = Duration::from_secs(1);
        assert!(Easing::Linear.progress(Duration::ZERO, second).abs() < f64::EPSILON);
        assert!((Easing::Linear.progress(second / 4, second) - 0.25).abs() < 1e-6);
        assert!((Easing::Linear.progress(second * 3, second) - 1.0).abs() < f64::EPSILON);
        assert!(
            (Easing::EaseOut.progress(Duration::ZERO, Duration::ZERO) - 1.0).abs() < f64::EPSILON
        );
    }

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
