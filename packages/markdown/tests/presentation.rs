//! Behavior of configurable Markdown presentation options.

use bmux_markdown::{
    MarkdownCodeBlockFrame, MarkdownParseOptions, MarkdownPresentation, MarkdownRenderOptions,
    MarkdownRenderResult, MarkdownSelectionContainer, MarkdownSelectionKind,
    MarkdownStreamingRenderState, MarkdownTableHeaderAlignment, parse_markdown_document,
    parse_markdown_document_with, render_markdown,
};
use bmux_tui::prelude::{Line, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Rendered rows without the trailing space word wrapping may leave.
fn rows(result: &MarkdownRenderResult) -> Vec<String> {
    result
        .lines
        .iter()
        .map(|line| line.plain_text().trim_end().to_owned())
        .collect()
}

fn render_rows(source: &str, options: &MarkdownRenderOptions) -> Vec<String> {
    rows(&render_markdown(source, options))
}

fn compact_presentation() -> MarkdownPresentation {
    MarkdownPresentation {
        list_bullet: "-".to_owned(),
        list_marker_gap: 1,
        code_block_frame: MarkdownCodeBlockFrame::None,
        blockquote_bar: "▎".to_owned(),
        table_header_alignment: MarkdownTableHeaderAlignment::Center,
        smart_punctuation: false,
    }
}

/// Text displayed in `rect` on its row.
fn displayed_in_rect(lines: &[Line], x: u16, y: u16, width: u16) -> String {
    let Some(line) = lines.get(usize::from(y)) else {
        return String::new();
    };
    let start = usize::from(x);
    let end = start + usize::from(width);
    let mut column = 0;
    let mut text = String::new();
    for grapheme in line.plain_text().graphemes(true) {
        if column >= start && column < end {
            text.push_str(grapheme);
        }
        column += grapheme.width();
    }
    text
}

/// Every non-whitespace text or code unit with display geometry must point at
/// cells showing exactly the source grapheme it claims.
fn assert_provenance_agrees(source: &str, result: &MarkdownRenderResult) -> usize {
    let mut checked = 0;
    for unit in result.selection_provenance() {
        if !matches!(
            unit.kind,
            MarkdownSelectionKind::Text | MarkdownSelectionKind::Code
        ) || unit.rects.is_empty()
        {
            continue;
        }
        let expected = unit
            .source_ranges
            .iter()
            .map(|range| &source[range.clone()])
            .collect::<String>();
        if expected.chars().all(char::is_whitespace) {
            continue;
        }
        let displayed = unit
            .rects
            .iter()
            .map(|rect| displayed_in_rect(&result.lines, rect.x, rect.y, rect.width))
            .collect::<String>();
        assert_eq!(displayed, expected, "unit {unit:?} in {:?}", rows(result));
        checked += 1;
    }
    checked
}

#[test]
fn default_presentation_matches_previous_glyphs() {
    let options = MarkdownRenderOptions::new(40);
    assert_eq!(options.presentation, MarkdownPresentation::default());
    assert!(MarkdownParseOptions::default().smart_punctuation);
    let source = "- one\n- two\n\n1. first\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n\"Hi\" don't -- go...";
    let explicit =
        MarkdownRenderOptions::new(40).with_presentation(MarkdownPresentation::default());
    assert_eq!(
        render_markdown(source, &options),
        render_markdown(source, &explicit)
    );
    assert_eq!(
        render_rows(source, &options),
        [
            "•  one",
            "•  two",
            "",
            "1.  first",
            "",
            "│ quoted",
            "",
            "┌─ rust",
            "│ fn main() {}",
            "└─",
            "",
            "“Hi” don’t – go…",
        ]
    );
}

#[test]
fn list_bullet_and_gap_set_markers_and_continuation_indent() {
    let options = MarkdownRenderOptions::new(12).with_presentation(compact_presentation());
    let source = "- alpha beta gamma delta\n\n1. one two three four";
    let rendered = render_rows(source, &options);
    assert_eq!(
        rendered,
        [
            "- alpha beta",
            "  gamma",
            "  delta",
            "",
            "1. one two",
            "   three",
            "   four",
        ]
    );
}

#[test]
fn wide_and_control_glyphs_are_measured_and_bounded() {
    for (bullet, bar) in [
        ("界", "界"),
        ("👩‍💻", "🧪"),
        ("e\u{301}", "\u{1b}[31m│"),
        ("\u{7}\n-\t", "\r"),
        ("", ""),
    ] {
        let presentation = MarkdownPresentation {
            list_bullet: bullet.to_owned(),
            list_marker_gap: 1,
            blockquote_bar: bar.to_owned(),
            ..MarkdownPresentation::default()
        };
        let source = "- alpha beta gamma\n\n> quoted words wrap here\n\n1. x";
        for width in 0..=14 {
            let result = render_markdown(
                source,
                &MarkdownRenderOptions::new(width).with_presentation(presentation.clone()),
            );
            let limit = usize::from(width.max(1));
            for row in &result.lines {
                let text = row.plain_text();
                assert!(row.width() <= limit, "{bullet:?} at {width}: {text:?}");
                assert!(!text.chars().any(char::is_control), "{text:?}");
            }
            assert!(
                result
                    .geometry
                    .iter()
                    .flat_map(|item| &item.rects)
                    .all(|rect| usize::from(rect.x + rect.width) <= limit)
            );
            assert_provenance_agrees(source, &result);
        }
    }
}

#[test]
fn wide_bullet_continuation_uses_measured_marker_width() {
    let presentation = MarkdownPresentation {
        list_bullet: "界".to_owned(),
        list_marker_gap: 1,
        ..MarkdownPresentation::default()
    };
    let rendered = render_rows(
        "- alpha beta gamma",
        &MarkdownRenderOptions::new(10).with_presentation(presentation),
    );
    assert_eq!(rendered, ["界 alpha", "   beta", "   gamma"]);
}

#[test]
fn blockquote_bar_prefixes_every_wrapped_row() {
    let options = MarkdownRenderOptions::new(12).with_presentation(compact_presentation());
    let source = "> quoted words that wrap";
    let result = render_markdown(source, &options);
    assert_eq!(rows(&result), ["▎ quoted", "▎ words that", "▎ wrap"]);
    assert!(assert_provenance_agrees(source, &result) > 0);
    assert!(
        result
            .selection_provenance()
            .iter()
            .flat_map(|unit| &unit.rects)
            .all(|rect| rect.x >= 2)
    );
}

#[test]
fn unframed_code_blocks_render_at_block_column_with_highlighting() {
    let options = MarkdownRenderOptions::new(40).with_presentation(compact_presentation());
    let source = "before\n\n```rust\nfn main() {\n    let x = 1;\n}\n```\n\nafter";
    let result = render_markdown(source, &options);
    assert_eq!(
        rows(&result),
        [
            "before",
            "",
            "fn main() {",
            "    let x = 1;",
            "}",
            "",
            "after",
        ]
    );
    assert!(
        result.lines[2]
            .spans
            .iter()
            .any(|span| span.content.contains("fn") && span.style != Style::new())
    );

    // No fence rows exist, so only the traversal identity is recorded.
    assert_eq!(result.code_block_geometry.len(), 1);
    assert_eq!(
        result.code_block_geometry[0].contribution_id,
        "code-block:0"
    );

    // Copy and selection of the block still resolve to canonical source.
    let blocks = result.code_block_selections();
    assert_eq!(blocks.len(), 1);
    let body = blocks[0].body_ranges[0].clone();
    assert_eq!(&source[body], "fn main() {\n    let x = 1;\n}\n");
    let code_units = result
        .selection_provenance()
        .iter()
        .filter(|unit| {
            unit.containers
                .contains(&MarkdownSelectionContainer::CodeBlock)
        })
        .filter(|unit| !unit.rects.is_empty())
        .collect::<Vec<_>>();
    assert!(!code_units.is_empty());
    assert!(
        code_units
            .iter()
            .flat_map(|unit| &unit.rects)
            .all(|rect| { (2..=4).contains(&rect.y) })
    );
    let first = code_units[0];
    assert_eq!(&source[first.source_ranges[0].clone()], "f");
    assert_eq!((first.rects[0].x, first.rects[0].y), (0, 2));
    assert!(assert_provenance_agrees(source, &result) > 0);
}

#[test]
fn framed_code_blocks_keep_header_and_footer_geometry() {
    let result = render_markdown("```rust\nx\n```", &MarkdownRenderOptions::new(40));
    let ids = result
        .code_block_geometry
        .iter()
        .map(|item| item.contribution_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["code-header:0", "code-footer:0", "code-block:0"]);
}

#[test]
fn unframed_code_in_list_starts_at_item_column() {
    let options = MarkdownRenderOptions::new(40).with_presentation(compact_presentation());
    let source = "- item\n\n  ```\n  code\n  ```";
    let result = render_markdown(source, &options);
    assert!(
        rows(&result).contains(&"  code".to_owned()),
        "{:?}",
        rows(&result)
    );
    assert!(assert_provenance_agrees(source, &result) > 0);
}

#[test]
fn unframed_code_blocks_degrade_at_tiny_widths() {
    let options_for =
        |width| MarkdownRenderOptions::new(width).with_presentation(compact_presentation());
    for width in 0..=6 {
        let result = render_markdown("```\n界👩‍💻abc\n```", &options_for(width));
        assert!(
            result
                .lines
                .iter()
                .all(|row| row.width() <= usize::from(width.max(1)))
        );
    }
}

#[test]
fn table_header_alignment_centers_header_cells() {
    let source = "| a | b |\n|:--|--:|\n| long | longer |";
    let column = render_rows(source, &MarkdownRenderOptions::new(40));
    assert_eq!(column[1], "│ a    │      b │");
    let centered = render_rows(
        source,
        &MarkdownRenderOptions::new(40).with_presentation(compact_presentation()),
    );
    assert_eq!(centered[1], "│  a   │   b    │");
    assert_eq!(centered[3], "│ long │ longer │");
}

#[test]
fn smart_punctuation_can_be_disabled() {
    let source = "\"Hi\" don't -- wait... ok";
    assert_eq!(
        render_rows(source, &MarkdownRenderOptions::new(80)),
        ["“Hi” don’t – wait… ok"]
    );
    let literal = MarkdownRenderOptions::new(80).with_presentation(compact_presentation());
    let result = render_markdown(source, &literal);
    assert_eq!(rows(&result), [source]);

    // Literal rendering maps every visible source grapheme back to its cell.
    let visible = source.chars().filter(|ch| !ch.is_whitespace()).count();
    assert_eq!(assert_provenance_agrees(source, &result), visible);

    let parsed = parse_markdown_document_with(
        source,
        &MarkdownParseOptions {
            smart_punctuation: false,
        },
    );
    assert_ne!(parsed, parse_markdown_document(source));
}

#[test]
fn presentation_changes_signature_and_streaming_reuse() {
    let source = "plain words that \"stream\" in";
    let default = MarkdownRenderOptions::new(40).with_streaming(true);
    let custom = default.clone().with_presentation(compact_presentation());
    assert_ne!(
        render_markdown(source, &default).layout_signature(),
        render_markdown(source, &custom).layout_signature()
    );

    let mut state = MarkdownStreamingRenderState::default();
    let first = state.render(source, &default);
    assert_eq!(rows(&first), ["plain words that “stream” in"]);
    let changed = state.render(source, &custom);
    assert_eq!(rows(&changed), [source]);
}

#[test]
fn streaming_provenance_parses_like_the_render() {
    let options = MarkdownRenderOptions::new(24)
        .with_streaming(true)
        .with_presentation(compact_presentation());
    let source = "first \"para\" here\n\nsecond don't stop -- now";
    let mut state = MarkdownStreamingRenderState::default();
    let mut streamed = None;
    for end in (1..=source.len()).filter(|end| source.is_char_boundary(*end)) {
        streamed = Some(state.render(&source[..end], &options));
    }
    let streamed = streamed.expect("streamed render");
    let full = render_markdown(source, &options);
    assert_eq!(streamed.lines, full.lines);
    assert_eq!(streamed.selection_provenance(), full.selection_provenance());
    assert!(assert_provenance_agrees(source, &streamed) > 0);
}

#[test]
fn nested_list_and_quote_provenance_maps_with_custom_presentation() {
    let source = "> - quoted item with several words\n>   - nested entry wraps too\n\n1. ordered \"words\" here";
    for width in [8, 12, 20, 40] {
        let result = render_markdown(
            source,
            &MarkdownRenderOptions::new(width).with_presentation(compact_presentation()),
        );
        assert!(assert_provenance_agrees(source, &result) > 0);
        assert!(
            result
                .lines
                .iter()
                .all(|row| row.width() <= usize::from(width))
        );
    }
}
