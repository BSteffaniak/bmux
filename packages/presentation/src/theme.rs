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
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Self::dark()),
            "light" => Some(Self::light()),
            _ => None,
        }
    }
}
