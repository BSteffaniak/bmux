use serde::{Deserialize, Serialize};

/// Terminal color representation used by the structured grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    /// ANSI indexed color.
    Indexed(u8),
    /// 24-bit RGB color.
    Rgb { r: u8, g: u8, b: u8 },
}

/// Interned style identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StyleId(pub u32);

impl StyleId {
    /// Default terminal style.
    pub const DEFAULT: Self = Self(0);
}

/// Display attributes for one or more terminal cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Terminal SGR attributes are independent bit flags.
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub dim: bool,
    pub strike: bool,
}

/// Small style interner. The default style is always id 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StylePalette {
    styles: Vec<Style>,
}

impl Default for StylePalette {
    fn default() -> Self {
        Self {
            styles: vec![Style::default()],
        }
    }
}

impl StylePalette {
    /// Build a palette from styles already encoded in id order.
    #[must_use]
    pub fn from_styles(styles: Vec<Style>) -> Self {
        if styles.is_empty() {
            Self::default()
        } else {
            Self { styles }
        }
    }

    /// Return an interned id for `style`.
    #[must_use]
    pub fn intern(&mut self, style: Style) -> StyleId {
        if let Some(index) = self.styles.iter().position(|candidate| *candidate == style) {
            return StyleId(u32::try_from(index).unwrap_or(u32::MAX));
        }
        self.styles.push(style);
        StyleId(u32::try_from(self.styles.len() - 1).unwrap_or(u32::MAX))
    }

    /// Resolve a style id.
    #[must_use]
    pub fn get(&self, id: StyleId) -> Style {
        usize::try_from(id.0)
            .ok()
            .and_then(|index| self.styles.get(index).copied())
            .unwrap_or_default()
    }

    /// All interned styles in id order.
    #[must_use]
    pub fn styles(&self) -> &[Style] {
        &self.styles
    }
}
