//! Generic source-code card and gutter rendering.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::{Color, Line, Span, Style};

use crate::selection::{
    ComponentSelectionOutcome, ComponentSelectionPolicy, ComponentSelectionState,
    paint_component_scope,
};
use unicode_width::UnicodeWidthStr;

/// Input used to render a source viewer card.
#[derive(Debug, Clone, Copy)]
pub struct SourceViewerInput<'a> {
    /// Label associated with the source text.
    pub label: &'a str,
    /// Optional caller-styled source lines.
    pub styled_lines: Option<&'a [Line]>,
    /// Source text to display.
    pub contents: &'a str,
    /// Absolute, one-based number of the first source line.
    pub start_line: usize,
    /// Maximum number of logical source lines to display.
    pub max_lines: usize,
    /// Message displayed when logical lines are omitted.
    pub truncated_message: &'a str,
    /// Whether to display line numbers.
    pub line_numbers: bool,
}

const SOURCE_CARD_MIN_WIDTH: usize = 16;
const SOURCE_CARD_UNNUMBERED_CHROME_WIDTH: usize = 4;
const SOURCE_CARD_NUMBERED_CHROME_WIDTH: usize = 7;

/// Semantic styles used by source viewer cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceViewerStyle {
    /// Base style patched beneath source and syntax token styles.
    pub source: Style,
    /// Card border style.
    pub border: Style,
    /// Line-number gutter style.
    pub gutter: Style,
    /// Omission and truncation message style.
    pub truncated: Style,
}

impl From<crate::theme::ComponentTheme> for SourceViewerStyle {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        Self {
            source: theme.text,
            border: theme.border,
            gutter: theme.muted,
            truncated: theme.muted,
        }
    }
}

impl Default for SourceViewerStyle {
    fn default() -> Self {
        let muted = Style::new().fg(Color::BrightBlack);
        Self {
            source: Style::new(),
            border: muted,
            gutter: muted,
            truncated: muted,
        }
    }
}

/// Register exact source-content mappings for a rendered source card.
///
/// Border, gutters, and truncation messages remain non-source chrome. Wrapped
/// source rows retain original UTF-8 byte offsets.
pub fn register_source_viewer_selection(
    input: SourceViewerInput<'_>,
    area: Rect,
    selection: &ComponentSelectionState,
    policy: &ComponentSelectionPolicy,
    cx: &mut PaintCx<'_, '_>,
) -> ComponentSelectionOutcome {
    let scope_outcome = paint_component_scope(cx, selection, policy, area, area);
    if !policy.enabled || area.is_empty() {
        return scope_outcome;
    }
    let projection = source_projection(input, area.width);
    let body_width = projection.body_width;
    let content_x = area.x.saturating_add(projection.source_column());
    let mut screen_y = area.y.saturating_add(1);
    let mut fragments = 0_usize;
    for row in &projection.rows {
        if screen_y >= area.bottom() {
            break;
        }
        let text = Line::from_spans(row.spans.clone()).plain_text();
        for fragment in bmux_tui::selection::plain_text_fragments(
            selection.scope_id.clone(),
            format!(
                "{}.line.{}",
                selection.scope_id.as_str(),
                input.start_line.saturating_add(row.line_index)
            ),
            Rect::new(
                content_x,
                screen_y,
                u16::try_from(body_width).unwrap_or(u16::MAX),
                1,
            ),
            u64::try_from(row.line_index).unwrap_or(u64::MAX),
            &text,
            row.source_offset,
            selection.revision,
        ) {
            cx.push_selection_fragment(fragment);
            fragments = fragments.saturating_add(1);
        }
        screen_y = screen_y.saturating_add(1);
    }
    if fragments == 0 {
        scope_outcome
    } else {
        ComponentSelectionOutcome::ContentRegistered { fragments }
    }
}

/// Render source text using the same card and gutter language as the diff viewer.
#[must_use]
pub fn source_viewer_rows(input: SourceViewerInput<'_>, width: u16) -> Vec<Line> {
    source_viewer_rows_with_style(input, width, SourceViewerStyle::default())
}

/// One source row before card chrome is applied.
#[derive(Debug)]
pub struct SourceRow {
    /// Styled source graphemes, excluding gutters and padding.
    pub spans: Vec<Span>,
    /// Zero-based logical line index in the input.
    pub line_index: usize,
    /// Original UTF-8 source byte offset, including preceding line endings.
    pub source_offset: usize,
    /// Whether this is the first wrapped row of the logical line.
    pub first: bool,
}

/// Derived source-card geometry shared by paint and producer-owned selection.
#[derive(Debug)]
pub struct SourceProjection {
    /// Source rows in paint order, beginning one row below the top border.
    pub rows: Vec<SourceRow>,
    /// Card width, excluding its two-cell outer indentation.
    pub card_width: u16,
    /// Source text width inside the card.
    pub body_width: usize,
    /// Width reserved for line numbers, or zero when disabled.
    pub number_width: usize,
    /// Whether an omission message follows the source rows.
    pub truncated: bool,
}

impl SourceProjection {
    /// Source column relative to the outer card allocation, excluding chrome.
    #[must_use]
    pub fn source_column(&self) -> u16 {
        4_u16
            .saturating_add(u16::try_from(self.number_width).unwrap_or(u16::MAX))
            .saturating_add(u16::from(self.number_width > 0) * 3)
    }
}

/// Project source and syntax annotations using canonical text geometry.
///
/// Producers can export selection provenance from these rows without parsing
/// rendered borders or reconstructing wrapping. Source remains caller-owned.
#[must_use]
pub fn source_projection(input: SourceViewerInput<'_>, width: u16) -> SourceProjection {
    let lines = input.contents.lines().collect::<Vec<_>>();
    let displayed = lines.len().min(input.max_lines);
    let last_line = input.start_line.saturating_add(displayed.saturating_sub(1));
    let number_width = if input.line_numbers {
        last_line.to_string().len().max(1)
    } else {
        0
    };
    let available_width = width.saturating_sub(2);
    let card_width = source_card_width(
        &lines[..displayed],
        (lines.len() > displayed).then_some(input.truncated_message),
        number_width,
        available_width,
    );
    let body_width = usize::from(card_width)
        .saturating_sub(source_card_chrome_width(number_width))
        .max(1);
    let mut rows = Vec::new();
    let mut source_offset = 0usize;
    for (index, source) in lines[..displayed].iter().enumerate() {
        // Syntax styles may annotate source, never replace it or omit lines.
        let spans = input
            .styled_lines
            .and_then(|styled| styled.get(index))
            .filter(|line| line.plain_text() == *source)
            .map_or_else(|| vec![Span::raw(*source)], |line| line.spans.clone());
        let text = bmux_tui::composition::TextBlock::new(bmux_tui::text::Text::from_lines([
            Line::from_spans(spans),
        ]))
        .wrap(bmux_tui::text::TextWrap::Character);
        let layout = text.layout(
            Constraints::for_width(body_width.try_into().unwrap_or(u64::MAX)),
            &mut LayoutCx::new(),
        );
        for (chunk_index, row) in text.projection(&layout).into_iter().enumerate() {
            rows.push(SourceRow {
                spans: row.line.spans,
                line_index: index,
                source_offset: source_offset.saturating_add(row.source_range.start),
                first: chunk_index == 0,
            });
        }
        // Preserve actual CRLF/LF source boundaries rather than assuming one byte.
        let remaining = &input.contents[source_offset..];
        source_offset = source_offset
            .saturating_add(remaining.find('\n').map_or(remaining.len(), |end| end + 1));
    }
    SourceProjection {
        rows,
        card_width,
        body_width,
        number_width,
        truncated: lines.len() > displayed,
    }
}

/// Render source text with caller-supplied semantic card styles.
#[must_use]
pub fn source_viewer_rows_with_style(
    input: SourceViewerInput<'_>,
    width: u16,
    style: SourceViewerStyle,
) -> Vec<Line> {
    source_projection(input, width).render_rows(input.start_line, input.truncated_message, style)
}

impl SourceProjection {
    /// Paint-ready rows from this exact projection, preserving its source geometry.
    #[must_use]
    pub fn render_rows(
        &self,
        start_line: usize,
        truncated_message: &str,
        style: SourceViewerStyle,
    ) -> Vec<Line> {
        let card_width = self.card_width;
        let number_width = self.number_width;
        let mut rows = vec![card_border(card_width, "┌", "┐", style.border)];
        for row in &self.rows {
            let spans = row
                .spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), style.source.patch(span.style)))
                .collect();
            let number =
                (row.first && number_width > 0).then(|| start_line.saturating_add(row.line_index));
            rows.push(source_card_row(
                spans,
                number,
                number_width,
                card_width,
                style,
            ));
        }
        if self.truncated {
            rows.push(source_card_row(
                vec![Span::styled(truncated_message, style.truncated)],
                None,
                number_width,
                card_width,
                style,
            ));
        }
        rows.push(card_border(card_width, "└", "┘", style.border));
        rows
    }
}

const fn source_card_chrome_width(number_width: usize) -> usize {
    if number_width == 0 {
        SOURCE_CARD_UNNUMBERED_CHROME_WIDTH
    } else {
        number_width.saturating_add(SOURCE_CARD_NUMBERED_CHROME_WIDTH)
    }
}

fn source_card_width(
    lines: &[&str],
    truncated_message: Option<&str>,
    number_width: usize,
    available_width: u16,
) -> u16 {
    let available = usize::from(available_width.max(1));
    let content_width = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(*line))
        .chain(truncated_message.map(UnicodeWidthStr::width))
        .max()
        .unwrap_or(0);
    let desired = content_width.saturating_add(source_card_chrome_width(number_width));
    u16::try_from(desired.clamp(SOURCE_CARD_MIN_WIDTH.min(available), available))
        .unwrap_or(u16::MAX)
}

fn source_card_row(
    content: Vec<Span>,
    line_number: Option<usize>,
    number_width: usize,
    width: u16,
    style: SourceViewerStyle,
) -> Line {
    let gutter = style.gutter;
    let mut card = vec![Span::styled("│ ", style.border)];
    if number_width > 0 {
        card.push(Span::styled(
            line_number.map_or_else(
                || " ".repeat(number_width),
                |number| format!("{number:>number_width$}"),
            ),
            gutter,
        ));
        card.push(Span::styled(" │ ", gutter));
    }
    card.extend(content);
    pad_card_spans(
        &mut card,
        usize::from(width).saturating_sub(2),
        style.source,
    );
    card.push(Span::styled(" │", style.border));
    Line::from_spans(
        std::iter::once(Span::styled("  ", style.border))
            .chain(card)
            .collect::<Vec<_>>(),
    )
}

fn card_border(width: u16, left: &str, right: &str, style: Style) -> Line {
    let inner = usize::from(width.saturating_sub(2));
    Line::from_spans(vec![
        Span::styled("  ", style),
        Span::styled(left, style),
        Span::styled("─".repeat(inner), style),
        Span::styled(right, style),
    ])
}

pub(crate) fn pad_card_spans(spans: &mut Vec<Span>, target_width: usize, style: Style) {
    let current_width = spans_width(spans);
    if current_width < target_width {
        spans.push(Span::styled(
            " ".repeat(target_width - current_width),
            style,
        ));
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::style::Modifier;

    fn rendered(rows: &[Line]) -> String {
        rows.iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn caller_styled_lines_are_preserved() {
        let token_style = Style::new().fg(Color::Red);
        let styled = [Line::from_spans(vec![Span::styled("let", token_style)])];
        let rows = source_viewer_rows(
            SourceViewerInput {
                label: "file.rs",
                styled_lines: Some(&styled),
                contents: "let",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: false,
            },
            40,
        );

        assert!(
            rows.iter()
                .flat_map(|row| &row.spans)
                .any(|span| span.style.fg == Some(Color::Red))
        );
    }

    #[test]
    fn base_source_style_stays_beneath_caller_token_styles() {
        let token_style = Style::new().fg(Color::Red);
        let styled = [Line::from_spans(vec![
            Span::styled("let", token_style),
            Span::raw(" value"),
        ])];
        let style = SourceViewerStyle {
            source: Style::new().fg(Color::Blue).add_modifier(Modifier::DIM),
            ..SourceViewerStyle::default()
        };
        let rows = source_viewer_rows_with_style(
            SourceViewerInput {
                label: "file.rs",
                styled_lines: Some(&styled),
                contents: "let value",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: false,
            },
            40,
            style,
        );
        let token = rows
            .iter()
            .flat_map(|row| &row.spans)
            .find(|span| span.content.starts_with('l'))
            .expect("token span");

        assert_eq!(token.style.fg, Some(Color::Red));
        assert!(token.style.modifiers.contains(Modifier::DIM));
        assert!(
            rows.iter()
                .flat_map(|row| &row.spans)
                .any(|span| span.content.contains('v') && span.style.fg == Some(Color::Blue))
        );
    }

    #[test]
    fn canonical_projection_keeps_graphemes_across_syntax_spans() {
        let styled = [Line::from_spans(vec![
            Span::styled("👩", Style::new().fg(Color::Red)),
            Span::raw("\u{200d}💻x"),
        ])];
        let projection = super::source_projection(
            SourceViewerInput {
                label: "source",
                styled_lines: Some(&styled),
                contents: "👩\u{200d}💻x",
                start_line: 1,
                max_lines: 1,
                truncated_message: "",
                line_numbers: false,
            },
            8,
        );
        assert_eq!(projection.rows.len(), 2);
        assert_eq!(
            Line::from_spans(projection.rows[0].spans.clone()).plain_text(),
            "👩\u{200d}💻"
        );
        assert_eq!(projection.rows[1].source_offset, "👩\u{200d}💻".len());
    }

    #[test]
    fn projection_preserves_source_boundaries_and_falls_back_from_incomplete_styles() {
        let styled = [Line::from("not the source")];
        let input = SourceViewerInput {
            styled_lines: Some(&styled),
            label: "source",
            contents: "a界\r\nsecond",
            start_line: 1,
            max_lines: 10,
            truncated_message: "truncated",
            line_numbers: false,
        };
        let projection = super::source_projection(input, 40);
        assert_eq!(projection.rows.len(), 2);
        assert_eq!(projection.rows[1].source_offset, "a界\r\n".len());
        assert_eq!(
            Line::from_spans(projection.rows[0].spans.clone()).plain_text(),
            "a界"
        );
        let painted = source_viewer_rows(input, 40);
        for (index, row) in projection.rows.iter().enumerate() {
            assert!(
                painted[index + 1]
                    .plain_text()
                    .contains(&Line::from_spans(row.spans.clone()).plain_text())
            );
        }
    }

    #[test]
    fn renders_absolute_aligned_line_numbers() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                styled_lines: None,
                label: "file.rs",
                contents: "nine\nten",
                start_line: 9,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            40,
        );
        let output = rendered(&rows);
        assert!(output.contains(" 9 │ nine"), "{output}");
        assert!(output.contains("10 │ ten"), "{output}");
    }

    #[test]
    fn rows_fit_available_width_and_keep_right_border() {
        let width = 24;
        let rows = source_viewer_rows(
            SourceViewerInput {
                styled_lines: None,
                label: "file.rs",
                contents: "a source line long enough to wrap",
                start_line: 42,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            width,
        );

        for row in &rows {
            let text = row
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert!(UnicodeWidthStr::width(text.as_str()) <= usize::from(width));
            assert!(text.ends_with('│') || text.ends_with('┐') || text.ends_with('┘'));
        }
    }

    #[test]
    fn short_source_uses_content_sized_card() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                styled_lines: None,
                label: "file.rs",
                contents: "let x = 1;",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            100,
        );

        assert!(line_width(&rows[0]) < 100, "{rows:?}");
        assert!(
            rows.iter()
                .all(|row| line_width(row) == line_width(&rows[0]))
        );
    }

    #[test]
    fn omitted_long_lines_do_not_expand_source_card() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                styled_lines: None,
                label: "file.rs",
                contents: "short\nthis omitted line is intentionally extremely long and should not size the card",
                start_line: 1,
                max_lines: 1,
                truncated_message: "truncated",
                line_numbers: true,
            },
            100,
        );

        assert!(line_width(&rows[0]) < 40, "{rows:?}");
    }

    #[test]
    fn unicode_source_width_uses_terminal_cells() {
        let rows = source_viewer_rows(
            SourceViewerInput {
                styled_lines: None,
                label: "file.txt",
                contents: "界界",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: false,
            },
            100,
        );

        assert_eq!(line_width(&rows[0]), SOURCE_CARD_MIN_WIDTH + 2);
    }

    fn line_width(line: &Line) -> usize {
        line.spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_str()))
            .sum()
    }

    #[test]
    fn applies_caller_supplied_source_card_styles() {
        let custom = Style::new().fg(Color::Magenta).bg(Color::Blue);
        let rows = source_viewer_rows_with_style(
            SourceViewerInput {
                styled_lines: None,
                label: "file.txt",
                contents: "content",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: true,
            },
            40,
            SourceViewerStyle {
                source: custom,
                border: custom,
                gutter: custom,
                truncated: custom,
            },
        );

        assert!(
            rows.iter()
                .flat_map(|row| &row.spans)
                .all(|span| { span.style.bg == Some(Color::Blue) })
        );
        assert!(
            rows.first()
                .into_iter()
                .flat_map(|row| &row.spans)
                .all(|span| span.style.fg == Some(Color::Magenta))
        );
    }

    #[test]
    fn supports_unnumbered_source_cards() {
        let output = rendered(&source_viewer_rows(
            SourceViewerInput {
                styled_lines: None,
                label: "artifact",
                contents: "content",
                start_line: 1,
                max_lines: 30,
                truncated_message: "truncated",
                line_numbers: false,
            },
            40,
        ));
        assert!(!output.contains("1 │"), "{output}");
    }

    #[test]
    fn selection_wraps_unicode_at_grapheme_boundaries_with_source_offsets() {
        let input = SourceViewerInput {
            styled_lines: None,
            label: "unicode",
            contents: "a界e\u{301}z",
            start_line: 1,
            max_lines: 30,
            truncated_message: "truncated",
            line_numbers: false,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 6));
        let mut frame = Frame::new(&mut buffer);
        let state = ComponentSelectionState::new("source");

        register_source_viewer_selection(
            input,
            Rect::new(0, 0, 10, 6),
            &state,
            &ComponentSelectionPolicy::content(),
            &mut PaintCx::new(&mut frame),
        );

        let fragments = frame.selection().fragments();
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment.source_range == (1..4))
        );
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment.source_range == (4..7))
        );
        assert!(fragments.iter().all(|fragment| {
            input.contents.is_char_boundary(fragment.source_range.start)
                && input.contents.is_char_boundary(fragment.source_range.end)
        }));
        assert!(fragments.iter().any(|fragment| fragment.area.y > 1));
    }
}
