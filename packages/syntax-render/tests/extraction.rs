use bmux_syntax_render::SyntaxHighlighter;
use bmux_tui::prelude::Span;

#[test]
fn extracted_highlighter_reconstructs_unicode_source_and_uses_canonical_spans() {
    let highlighter = SyntaxHighlighter::new();
    let lines = ["// 東京 🧪", "let greeting = \"é\";"];
    let highlighted = highlighter.highlight_lines("rust", &lines);
    let _: &[Vec<Span>] = &highlighted;
    assert_eq!(highlighted.len(), lines.len());
    for (spans, source) in highlighted.iter().zip(lines) {
        let reconstructed = spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert_eq!(reconstructed, source);
    }

    let unknown = "plain 東京 🧪";
    let spans = highlighter.highlight_line_tokens("unregistered-language", unknown);
    assert_eq!(
        spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>(),
        unknown,
    );
}
