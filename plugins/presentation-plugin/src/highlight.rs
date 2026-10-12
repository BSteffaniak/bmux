pub(crate) fn highlight(deck: &bmux_presentation::Deck) -> BTreeMap<String, bmux_tui::text::Text> {
    let highlighter = bmux_syntax_render::SyntaxHighlighter::new();
    fn visit(
        blocks: &[Block],
        highlighter: &bmux_syntax_render::SyntaxHighlighter,
        output: &mut BTreeMap<String, bmux_tui::text::Text>,
    ) {
        for block in blocks {
            match &block.kind {
                BlockKind::Code { language, source } => {
                    let lines = source
                        .lines()
                        .map(|line| {
                            let spans = highlighter
                                .highlight_line_tokens(language.as_deref().unwrap_or("text"), line)
                                .into_iter()
                                .map(|token| {
                                    let mut style = bmux_tui::style::Style::new();
                                    style = match token.style.foreground {
                                        bmux_syntax_render::SyntaxColor::Default => style,
                                        bmux_syntax_render::SyntaxColor::Indexed(index) => {
                                            style.fg(bmux_tui::style::Color::Indexed(index))
                                        }
                                        bmux_syntax_render::SyntaxColor::Rgb(r, g, b) => {
                                            style.fg(bmux_tui::style::Color::Rgb(r, g, b))
                                        }
                                        bmux_syntax_render::SyntaxColor::Ansi(color) => {
                                            style.fg(ansi(color))
                                        }
                                    };
                                    if token.style.bold {
                                        style = style.add_modifier(bmux_tui::style::Modifier::BOLD);
                                    }
                                    if token.style.italic {
                                        style =
                                            style.add_modifier(bmux_tui::style::Modifier::ITALIC);
                                    }
                                    if token.style.underline {
                                        style = style
                                            .add_modifier(bmux_tui::style::Modifier::UNDERLINE);
                                    }
                                    bmux_tui::text::Span::styled(token.content, style)
                                })
                                .collect::<Vec<_>>();
                            bmux_tui::text::Line::from_spans(spans)
                        })
                        .collect::<Vec<_>>();
                    output.insert(
                        block.id.as_str().into(),
                        bmux_tui::text::Text::from_lines(lines),
                    );
                }
                BlockKind::Columns { columns } => {
                    for column in columns {
                        visit(&column.blocks, highlighter, output);
                    }
                }
                _ => (),
            }
        }
    }
    let mut output = BTreeMap::new();
    for slide in &deck.slides {
        visit(&slide.blocks, &highlighter, &mut output);
    }
    output
}
fn ansi(value: bmux_syntax_render::AnsiColor) -> bmux_tui::style::Color {
    use bmux_syntax_render::AnsiColor as A;
    use bmux_tui::style::Color as C;
    match value {
        A::Black => C::Black,
        A::Red => C::Red,
        A::Green => C::Green,
        A::Yellow => C::Yellow,
        A::Blue => C::Blue,
        A::Magenta => C::Magenta,
        A::Cyan => C::Cyan,
        A::White => C::White,
        A::BrightBlack => C::BrightBlack,
        A::BrightRed => C::BrightRed,
        A::BrightGreen => C::BrightGreen,
        A::BrightYellow => C::BrightYellow,
        A::BrightBlue => C::BrightBlue,
        A::BrightMagenta => C::BrightMagenta,
        A::BrightCyan => C::BrightCyan,
        A::BrightWhite => C::BrightWhite,
    }
}
