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
    /// Conceal glyphs while retaining their occupied cells and background.
    #[serde(default)]
    pub hidden: bool,
}

impl Style {
    /// Style used to fill cells erased or newly exposed while this style is
    /// active.
    ///
    /// This implements background color erase (BCE), which the `bmux-256color`
    /// terminfo entry advertises through `xterm-256color`. Applications rely on
    /// it to paint a region by setting a background and then erasing (`EL`,
    /// `ED`, `ECH`) or scrolling, instead of writing spaces across every column.
    ///
    /// `bg` is the erase color and is always preserved. `fg` is preserved only
    /// alongside `inverse`, because inverse video makes the foreground the
    /// effective background. Glyph-only attributes (`bold`, `italic`,
    /// `underline`, `dim`, `strike`) must not persist into erased cells, since
    /// they would otherwise decorate the blank spaces.
    ///
    /// When the active style contributes no background, this returns
    /// [`Style::default`] so erased cells stay default-styled and remain
    /// discardable trailing blanks.
    #[must_use]
    pub const fn erase_fill(self) -> Self {
        if self.inverse {
            return Self {
                fg: self.fg,
                bg: self.bg,
                inverse: true,
                bold: false,
                italic: false,
                underline: false,
                dim: false,
                strike: false,
                hidden: false,
            };
        }
        Self {
            fg: None,
            bg: self.bg,
            inverse: false,
            bold: false,
            italic: false,
            underline: false,
            dim: false,
            strike: false,
            hidden: false,
        }
    }
}

/// Small style interner. The default style is always id 0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StylePalette {
    styles: std::sync::Arc<Vec<Style>>,
    /// Each style's id, for the styles up to `covered`: shared like the styles, and rebuilt for
    /// any it does not yet cover (a deserialized or shared palette starts with none).
    #[serde(skip)]
    index: std::sync::Arc<StyleIndex>,
}

#[derive(Debug, Clone, Default)]
struct StyleIndex {
    ids: std::collections::HashMap<Style, u32>,
    covered: usize,
}

impl PartialEq for StylePalette {
    fn eq(&self, other: &Self) -> bool {
        self.styles == other.styles
    }
}

impl Eq for StylePalette {}

impl Default for StylePalette {
    fn default() -> Self {
        Self::from_shared_styles(std::sync::Arc::new(vec![Style::default()]))
    }
}

impl StylePalette {
    /// Build a palette from styles already encoded in id order.
    #[must_use]
    pub fn from_styles(styles: Vec<Style>) -> Self {
        Self::from_shared_styles(std::sync::Arc::new(styles))
    }

    /// Share an immutable id-ordered palette without copying its entries.
    #[must_use]
    pub fn from_shared_styles(styles: std::sync::Arc<Vec<Style>>) -> Self {
        let styles = if styles.is_empty() {
            std::sync::Arc::new(vec![Style::default()])
        } else {
            styles
        };
        Self {
            styles,
            index: std::sync::Arc::default(),
        }
    }

    /// Return an interned id for `style`: its first id when the palette holds it.
    #[must_use]
    pub fn intern(&mut self, style: Style) -> StyleId {
        if self.index.covered < self.styles.len() {
            let index = std::sync::Arc::make_mut(&mut self.index);
            for (id, candidate) in self.styles.iter().enumerate().skip(index.covered) {
                index
                    .ids
                    .entry(*candidate)
                    .or_insert(u32::try_from(id).unwrap_or(u32::MAX));
            }
            index.covered = self.styles.len();
        }
        if let Some(id) = self.index.ids.get(&style) {
            return StyleId(*id);
        }
        let id = u32::try_from(self.styles.len()).unwrap_or(u32::MAX);
        std::sync::Arc::make_mut(&mut self.styles).push(style);
        let index = std::sync::Arc::make_mut(&mut self.index);
        index.ids.insert(style, id);
        index.covered = self.styles.len();
        StyleId(id)
    }

    /// Resolve a style id.
    #[must_use]
    pub fn get(&self, id: StyleId) -> Style {
        usize::try_from(id.0)
            .ok()
            .and_then(|index| self.styles.get(index).copied())
            .unwrap_or_default()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.styles.capacity()
    }

    /// Styles held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    /// Whether the palette holds no style.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }

    /// Append `style` at the next id, as a replica copies its source's palette in order.
    pub(crate) fn push(&mut self, style: Style) {
        std::sync::Arc::make_mut(&mut self.styles).push(style);
    }

    /// All interned styles in id order.
    #[must_use]
    pub fn styles(&self) -> &[Style] {
        &self.styles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interning finds styles a palette was built or deserialized with, keeping a duplicated
    /// style's first id, and gives a new style the next id.
    #[test]
    fn interning_finds_existing_styles_by_their_first_id() {
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let red = Style {
            fg: Some(Color::Indexed(1)),
            ..Style::default()
        };
        let mut built = StylePalette::from_styles(vec![Style::default(), bold, red, bold]);
        assert_eq!(built.intern(bold), StyleId(1));
        assert_eq!(built.intern(red), StyleId(2));
        let wire = serde_json::to_string(&built).unwrap();
        let mut restored: StylePalette = serde_json::from_str(&wire).unwrap();
        assert_eq!(restored, built);
        assert_eq!(restored.intern(bold), StyleId(1));
        let italic = Style {
            italic: true,
            ..Style::default()
        };
        assert_eq!(restored.intern(italic), StyleId(4));
        assert_eq!(restored.intern(italic), StyleId(4));
        assert_eq!(restored.get(StyleId(4)), italic);
    }

    #[test]
    fn palette_clones_share_until_mutation_and_preserve_wire_shape() {
        let mut palette = StylePalette::default();
        let saved = palette.clone();
        assert!(std::sync::Arc::ptr_eq(&saved.styles, &palette.styles));
        assert_eq!(palette.intern(Style::default()), StyleId::DEFAULT);
        assert!(std::sync::Arc::ptr_eq(&saved.styles, &palette.styles));
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let id = palette.intern(bold);
        assert!(!std::sync::Arc::ptr_eq(&saved.styles, &palette.styles));
        assert_eq!(saved.styles().len(), 1);
        assert_eq!(palette.get(id), bold);
        let wire = serde_json::to_value(&palette).unwrap();
        assert_eq!(
            wire["styles"],
            serde_json::to_value(palette.styles()).unwrap()
        );
        assert_eq!(
            serde_json::from_value::<StylePalette>(wire).unwrap(),
            palette
        );
    }
}
