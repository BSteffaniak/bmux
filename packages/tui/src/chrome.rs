//! Panel, border, and modal chrome primitives.

use crate::geometry::Insets;
use crate::style::Style;

/// Border glyph set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderSet {
    /// Top-left corner.
    pub top_left: char,
    /// Top-right corner.
    pub top_right: char,
    /// Bottom-left corner.
    pub bottom_left: char,
    /// Bottom-right corner.
    pub bottom_right: char,
    /// Horizontal edge.
    pub horizontal: char,
    /// Vertical edge.
    pub vertical: char,
}

impl BorderSet {
    /// Single-line border glyphs.
    pub const SINGLE: Self = Self {
        top_left: '┌',
        top_right: '┐',
        bottom_left: '└',
        bottom_right: '┘',
        horizontal: '─',
        vertical: '│',
    };

    /// Rounded border glyphs.
    pub const ROUNDED: Self = Self {
        top_left: '╭',
        top_right: '╮',
        bottom_left: '╰',
        bottom_right: '╯',
        horizontal: '─',
        vertical: '│',
    };

    /// ASCII-safe border glyphs.
    pub const ASCII: Self = Self {
        top_left: '+',
        top_right: '+',
        bottom_left: '+',
        bottom_right: '+',
        horizontal: '-',
        vertical: '|',
    };

    /// Double-line border glyphs.
    pub const DOUBLE: Self = Self {
        top_left: '╔',
        top_right: '╗',
        bottom_left: '╚',
        bottom_right: '╝',
        horizontal: '═',
        vertical: '║',
    };

    /// Thick border glyphs.
    pub const THICK: Self = Self {
        top_left: '┏',
        top_right: '┓',
        bottom_left: '┗',
        bottom_right: '┛',
        horizontal: '━',
        vertical: '┃',
    };
}

/// Border side selection.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderSides {
    /// Render the top edge.
    pub top: bool,
    /// Render the right edge.
    pub right: bool,
    /// Render the bottom edge.
    pub bottom: bool,
    /// Render the left edge.
    pub left: bool,
}

impl BorderSides {
    /// All border sides.
    pub const ALL: Self = Self::new(true, true, true, true);
    /// No border sides.
    pub const NONE: Self = Self::new(false, false, false, false);
    /// Top border side only.
    pub const TOP: Self = Self::new(true, false, false, false);
    /// Right border side only.
    pub const RIGHT: Self = Self::new(false, true, false, false);
    /// Bottom border side only.
    pub const BOTTOM: Self = Self::new(false, false, true, false);
    /// Left border side only.
    pub const LEFT: Self = Self::new(false, false, false, true);

    /// Create a border side selection.
    #[allow(clippy::fn_params_excessive_bools)]
    #[must_use]
    pub const fn new(top: bool, right: bool, bottom: bool, left: bool) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// Horizontal-only border sides.
    #[must_use]
    pub const fn horizontal() -> Self {
        Self::new(true, false, true, false)
    }

    /// Vertical-only border sides.
    #[must_use]
    pub const fn vertical() -> Self {
        Self::new(false, true, false, true)
    }

    /// Insets occupied by these border sides.
    #[must_use]
    pub const fn insets(self) -> Insets {
        Insets::new(
            if self.top { 1 } else { 0 },
            if self.right { 1 } else { 0 },
            if self.bottom { 1 } else { 0 },
            if self.left { 1 } else { 0 },
        )
    }
}

impl Default for BorderSides {
    fn default() -> Self {
        Self::ALL
    }
}

/// Border configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Border {
    /// Border glyphs.
    pub set: BorderSet,
    /// Border style.
    pub style: Style,
    /// Border sides to render.
    pub sides: BorderSides,
}

impl Border {
    /// Create a border with a glyph set and style.
    #[must_use]
    pub const fn new(set: BorderSet, style: Style) -> Self {
        Self {
            set,
            style,
            sides: BorderSides::ALL,
        }
    }

    /// Create a single-line border with default style.
    #[must_use]
    pub const fn single() -> Self {
        Self::new(BorderSet::SINGLE, Style::new())
    }

    /// Create a rounded border with default style.
    #[must_use]
    pub const fn rounded() -> Self {
        Self::new(BorderSet::ROUNDED, Style::new())
    }

    /// Create an ASCII-safe border with default style.
    #[must_use]
    pub const fn ascii() -> Self {
        Self::new(BorderSet::ASCII, Style::new())
    }

    /// Create a double-line border with default style.
    #[must_use]
    pub const fn double() -> Self {
        Self::new(BorderSet::DOUBLE, Style::new())
    }

    /// Create a thick border with default style.
    #[must_use]
    pub const fn thick() -> Self {
        Self::new(BorderSet::THICK, Style::new())
    }

    /// Set border sides.
    #[must_use]
    pub const fn sides(mut self, sides: BorderSides) -> Self {
        self.sides = sides;
        self
    }

    /// Set border style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }
}
