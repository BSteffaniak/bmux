//! Terminal UI style primitives.

/// Terminal color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
}

#[cfg(test)]
mod tests {
    use super::{Color, Modifier, Style};

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
}
