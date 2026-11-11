//! Behavior tests for whole-text highlighting and diff projection.

use super::*;
use crate::SyntaxPalette;

fn roles(text: &HighlightedText, line: u32, content: &str) -> Vec<(String, SyntaxRole)> {
    text.line(line)
        .expect("line")
        .iter()
        .map(|token| (content[token.range()].to_owned(), token.style.role))
        .collect()
}

fn role_at(text: &HighlightedText, line: u32, byte: usize) -> Option<SyntaxRole> {
    text.line(line)?
        .iter()
        .find(|token| token.range().contains(&byte))
        .map(|token| token.style.role)
}

#[test]
fn tokens_cover_every_line_in_order_without_gaps() {
    let source = "fn main() {\r\n    let x = \"東京 😀\";\n\n    // e\u{301}\n}\n";
    let text = SyntaxHighlighter::new().highlight_text("rust", source);
    let lines = source
        .split_inclusive('\n')
        .map(|line| line.trim_end_matches(['\n', '\r']))
        .collect::<Vec<_>>();
    assert_eq!(text.line_count(), lines.len());
    for (index, content) in lines.iter().enumerate() {
        let tokens = text.line_at(index).expect("line");
        let mut offset = 0;
        for token in tokens {
            assert_eq!(token.start, offset, "{content:?}");
            assert!(token.end > token.start);
            assert!(content.is_char_boundary(token.start) && content.is_char_boundary(token.end));
            offset = token.end;
        }
        assert_eq!(offset, content.len(), "{content:?}");
    }
    assert_eq!(text.line_at(2).expect("empty line"), []);
}

#[test]
fn multi_line_constructs_keep_their_state_across_lines() {
    let source = "/* start\nstill comment fn x\nend */\nfn main() {}\n";
    let text = SyntaxHighlighter::new().highlight_text("rust", source);
    assert!(
        roles(&text, 2, "still comment fn x")
            .iter()
            .all(|(_, role)| *role == SyntaxRole::Comment)
    );
    assert_eq!(role_at(&text, 4, 0), Some(SyntaxRole::Keyword));
}

#[test]
fn line_comments_end_at_their_line() {
    let text = SyntaxHighlighter::new().highlight_text("rust", "// note\nfn main() {}");
    assert_eq!(role_at(&text, 1, 3), Some(SyntaxRole::Comment));
    assert_eq!(role_at(&text, 2, 0), Some(SyntaxRole::Keyword));
}

#[test]
fn whole_text_lines_match_line_by_line_reconstruction() {
    let source = "let greeting = \"é\";\n// 東京 🧪\n";
    let text = SyntaxHighlighter::new().highlight_text("rust", source);
    for (index, content) in source.lines().enumerate() {
        let number = u32::try_from(index + 1).unwrap();
        let spans = text.line_spans(number, content).expect("matching line");
        let rebuilt = spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert_eq!(rebuilt, content);
    }
}

#[test]
fn lookups_are_one_based_and_bounded() {
    let text = SyntaxHighlighter::new().highlight_text("rust", "a\nb\n");
    assert_eq!(
        text.line_count(),
        2,
        "a final terminator does not start a line"
    );
    assert!(text.line(0).is_none());
    assert!(text.line(1).is_some());
    assert!(text.line(3).is_none());
    assert!(
        SyntaxHighlighter::new()
            .highlight_text("rust", "")
            .is_empty()
    );
}

#[test]
fn mismatched_content_yields_no_tokens() {
    let text = SyntaxHighlighter::new().highlight_text("rust", "let a = 1;\n");
    assert!(text.line_matching(1, "let a = 1;").is_some());
    assert!(text.line_matching(1, "let a = 1;\r").is_some());
    assert!(text.line_matching(1, "let a = 2;").is_none());
    assert!(text.line_matching(2, "let a = 1;").is_none());
}

#[test]
fn unknown_syntaxes_produce_plain_tokens() {
    let palette = SyntaxPalette::default();
    let text = SyntaxHighlighter::with_palette(palette)
        .highlight_text("not-a-language", "plain 東京\n\nmore");
    let line = text.line(1).expect("line");
    assert_eq!(line.len(), 1);
    assert_eq!(line[0].style.role, SyntaxRole::Text);
    assert_eq!(line[0].style.foreground, palette.text);
    assert_eq!(text.line(2).expect("empty"), []);
}

#[test]
fn diff_lines_project_from_the_correct_side() {
    let old = "/* old\nshared\n*/\nfn removed() {}\n";
    let new = "/* new\nshared\n*/\nfn added() {}\n";
    let syntax = DiffSyntax::highlight(&SyntaxHighlighter::new(), "rust", Some(old), Some(new));

    let removed = syntax
        .diff_line(Some(4), None, "fn removed() {}")
        .expect("old side");
    assert_eq!(
        removed,
        syntax.side(DiffSide::Old).unwrap().line(4).unwrap()
    );

    let added = syntax
        .diff_line(None, Some(4), "fn added() {}")
        .expect("new side");
    assert_eq!(added, syntax.side(DiffSide::New).unwrap().line(4).unwrap());

    let context = syntax
        .diff_line(Some(2), Some(2), "shared")
        .expect("context");
    assert!(
        context
            .iter()
            .all(|token| token.style.role == SyntaxRole::Comment)
    );

    assert!(syntax.diff_line(None, Some(4), "fn removed() {}").is_none());
}

#[test]
fn absent_sides_fall_back_or_yield_nothing() {
    let syntax =
        DiffSyntax::highlight(&SyntaxHighlighter::new(), "rust", None, Some("fn a() {}\n"));
    assert!(syntax.side(DiffSide::Old).is_none());
    assert!(syntax.diff_line(Some(1), None, "fn a() {}").is_none());
    assert!(syntax.diff_line(Some(1), Some(1), "fn a() {}").is_some());

    let old_only = DiffSyntax::new(
        Some(SyntaxHighlighter::new().highlight_text("rust", "fn a() {}\n")),
        None,
    );
    assert!(
        old_only.diff_line(Some(1), Some(1), "fn a() {}").is_some(),
        "falls back to old"
    );
}

#[test]
fn results_can_be_computed_off_the_render_thread() {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SyntaxHighlighter>();
    assert_send_sync::<HighlightedText>();
    assert_send_sync::<DiffSyntax>();

    let highlighter = SyntaxHighlighter::new();
    let worker = std::thread::spawn(move || {
        DiffSyntax::highlight(&highlighter, "rust", Some("fn a() {}"), Some("fn b() {}"))
    });
    let syntax = worker.join().expect("worker");
    let local = DiffSyntax::highlight(
        &SyntaxHighlighter::new(),
        "rust",
        Some("fn a() {}"),
        Some("fn b() {}"),
    );
    assert_eq!(
        syntax, local,
        "highlighting is a pure function of its inputs"
    );
}

#[test]
fn styles_convert_to_terminal_styles() {
    let text = SyntaxHighlighter::new().highlight_text("markdown", "# Title\n");
    let token = *text
        .line(1)
        .expect("line")
        .iter()
        .find(|token| token.range().contains(&2))
        .expect("title token");
    assert!(token.style.bold);
    let style = token.style.to_tui();
    assert_eq!(style.fg, Some(token.style.foreground.to_tui()));
    assert!(style.modifiers.contains(bmux_tui::prelude::Modifier::BOLD));
}
