//! Presentation-specific styling above the domain-neutral TUI.
use bmux_markdown::MarkdownTheme;
use bmux_tui::prelude::{Color, Modifier, Style};

#[derive(Clone, Debug)]
pub struct PresentationTheme {
    pub name: String,
    pub text: Style,
    pub background: Style,
    pub footer: Style,
    pub diagnostic: Style,
    pub markdown: MarkdownTheme,
    pub block_gap: u64,
    pub column_gap: u64,
    pub margin: u64,
}
impl Default for PresentationTheme {
    fn default() -> Self {
        Self::dark()
    }
}
impl PresentationTheme {
    pub fn dark() -> Self {
        let text = Style::new().fg(Color::White);
        Self {
            name: "dark".into(),
            text,
            background: Style::new().bg(Color::Black),
            footer: Style::new().fg(Color::BrightBlack),
            diagnostic: Style::new().fg(Color::Yellow),
            markdown: MarkdownTheme {
                text,
                ..MarkdownTheme::default()
            },
            block_gap: 1,
            column_gap: 2,
            margin: 1,
        }
    }
    pub fn light() -> Self {
        let text = Style::new().fg(Color::Black);
        Self {
            name: "light".into(),
            text,
            background: Style::new().bg(Color::White),
            footer: Style::new().fg(Color::BrightBlack),
            diagnostic: Style::new().fg(Color::Red),
            markdown: MarkdownTheme {
                text,
                heading: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                ..MarkdownTheme::default()
            },
            block_gap: 1,
            column_gap: 2,
            margin: 1,
        }
    }
    /// Resolve safe built-in deck themes consistently across local and host rendering.
    /// Unknown names retain the existing default; custom themes require host policy.
    pub fn for_deck(deck: &crate::model::Deck) -> Self {
        deck.metadata
            .theme
            .as_deref()
            .and_then(Self::named)
            .unwrap_or_default()
    }
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Self::dark()),
            "light" => Some(Self::light()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deck_named_theme_switching_is_consistent() {
        let mut deck = crate::builder::DeckBuilder::new("d").build().unwrap();
        for name in ["light", "dark", "light"] {
            deck.metadata.theme = Some(name.into());
            let theme = PresentationTheme::for_deck(&deck);
            assert_eq!(theme.name, name);
            assert_eq!(
                theme.background,
                PresentationTheme::named(name).unwrap().background
            );
        }
        deck.metadata.theme = None;
        assert_eq!(PresentationTheme::for_deck(&deck).name, "dark");
        assert!(PresentationTheme::named("unsupported").is_none());
    }
}
