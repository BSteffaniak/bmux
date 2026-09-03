//! Generic bounded terminal transcript viewer component.

use bmux_terminal_grid::{
    Color as GridColor, GridLimits, PhysicalRow, Style as GridStyle, TerminalGrid,
    TerminalGridStream,
};
use bmux_tui::ansi::ansi_to_lines;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::{Color, Line, Span, Style};
use bmux_tui::selection::SelectionCapture;
use bmux_tui::style::Modifier;

use crate::selection::{
    ComponentSelectionOutcome, ComponentSelectionPolicy, ComponentSelectionState,
    paint_component_scope,
};

/// Default maximum number of terminal rows rendered inline.
pub const MAX_INLINE_TERMINAL_ROWS: usize = 28;

/// Stateful sizing policy for live terminal previews.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalViewerLiveState {
    visible_rows: usize,
}

impl TerminalViewerLiveState {
    /// Return the currently reserved live terminal rows.
    #[must_use]
    pub const fn visible_rows(self) -> usize {
        self.visible_rows
    }

    /// Grow the reserved live terminal rows from an already-decoded row count.
    pub fn update_rows(&mut self, content_rows: usize, max_rows: usize) {
        self.visible_rows = self.visible_rows.max(content_rows).min(max_rows);
    }

    /// Grow the reserved live terminal rows to fit `input`, capped by `max_rows`.
    pub fn update(&mut self, input: TerminalViewerInput<'_>, max_rows: usize) {
        let content_rows = terminal_viewer_content_row_count(input, max_rows);
        self.update_rows(content_rows, max_rows);
    }
}

/// Terminal row sizing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalViewerSizing {
    /// Render only current compact transcript content.
    Compact,
    /// Render a live preview with a stable, caller-managed row reservation.
    Live {
        visible_rows: usize,
        max_rows: usize,
    },
}

/// Register an isolated selection scope over decoded terminal grid text.
///
/// The logical document is the terminal emulator's visible text projection,
/// not the raw ANSI/control stream. Status and truncation chrome are excluded.
pub fn register_terminal_viewer_selection(
    input: TerminalViewerInput<'_>,
    area: Rect,
    selection: &ComponentSelectionState,
    policy: &ComponentSelectionPolicy,
    cx: &mut PaintCx<'_, '_>,
) -> ComponentSelectionOutcome {
    let isolated_policy = ComponentSelectionPolicy {
        enabled: policy.enabled,
        content_capture: policy.content_capture,
        chrome_capture: SelectionCapture::Disabled,
        auto_scroll: policy.auto_scroll,
    };
    let content_start = terminal_viewer_chrome_row_count(&input, area.width);
    let content_area = Rect::new(
        area.x,
        area.y.saturating_add(content_start),
        area.width,
        area.height.saturating_sub(content_start),
    );
    let scope_outcome = paint_component_scope(cx, selection, &isolated_policy, area, content_area);
    if !policy.enabled || content_area.is_empty() {
        return scope_outcome;
    }
    let lines = terminal_output_lines(&input);
    let mut source_offset = 0_usize;
    let mut fragments = 0_usize;
    for (index, line) in lines
        .iter()
        .enumerate()
        .take(usize::from(content_area.height))
    {
        let text = line.plain_text();
        for fragment in bmux_tui::selection::plain_text_fragments(
            selection.scope_id.clone(),
            format!("{}.grid", selection.scope_id.as_str()),
            Rect::new(
                content_area.x.saturating_add(4),
                content_area
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                content_area.width.saturating_sub(4),
                1,
            ),
            u64::try_from(index).unwrap_or(u64::MAX),
            &text,
            source_offset,
            selection.revision,
        ) {
            cx.push_selection_fragment(fragment);
            fragments = fragments.saturating_add(1);
        }
        source_offset = source_offset.saturating_add(text.len().saturating_add(1));
    }
    if fragments == 0 {
        scope_outcome
    } else {
        ComponentSelectionOutcome::ContentRegistered { fragments }
    }
}

/// Input used to render terminal transcript rows.
#[derive(Debug, Clone, Copy)]
pub struct TerminalViewerInput<'a> {
    /// Raw terminal stream output.
    pub output: &'a str,
    /// Terminal columns used when capturing the stream.
    pub columns: u16,
    /// Terminal rows used when capturing the stream.
    pub rows: u16,
    /// Process exit code, when known.
    pub exit_code: Option<i32>,
    /// Whether execution timed out, when known.
    pub timed_out: Option<bool>,
    /// Human-readable elapsed duration, when known.
    pub elapsed: Option<&'a str>,
    /// Whether to render a status summary before terminal rows.
    pub show_status: bool,
    /// Whether earlier output was omitted.
    pub output_truncated: bool,
    /// Original output byte length, when known.
    pub output_bytes: Option<u64>,
    /// Retained output byte length, when known.
    pub retained_output_bytes: Option<u64>,
    /// Terminal row sizing policy.
    pub sizing: TerminalViewerSizing,
}

/// Render terminal transcript rows using terminal-grid semantics.
#[must_use]
pub fn terminal_viewer_rows(input: TerminalViewerInput<'_>, width: u16) -> Vec<Line> {
    let mut rows = Vec::new();
    if input.show_status {
        push_wrapped_styled_text(
            &mut rows,
            vec![Span::styled("  ", muted_style())],
            &terminal_status(&input),
            width,
            terminal_status_style(&input),
            muted_style(),
        );
    }
    if input.output_truncated {
        push_wrapped_styled_text(
            &mut rows,
            vec![Span::styled("  ", muted_style())],
            &terminal_truncation_status(&input),
            width,
            muted_style(),
            muted_style(),
        );
    }
    for line in terminal_output_lines(&input) {
        rows.push(prefix_line(line, "    ", muted_style()));
    }
    rows
}

fn terminal_status(input: &TerminalViewerInput<'_>) -> String {
    let status = if input.timed_out.unwrap_or(false) {
        "timed out".to_owned()
    } else if let Some(exit_code) = input.exit_code {
        if exit_code == 0 {
            "completed".to_owned()
        } else {
            format!("failed · exit {exit_code}")
        }
    } else {
        "running".to_owned()
    };
    input.elapsed.map_or_else(
        || status.clone(),
        |elapsed| format!("{status} · duration {elapsed}"),
    )
}

fn terminal_status_style(input: &TerminalViewerInput<'_>) -> Style {
    if input.timed_out.unwrap_or(false) || input.exit_code.is_some_and(|code| code != 0) {
        Style::new().fg(Color::Red)
    } else {
        muted_style()
    }
}

fn terminal_truncation_status(input: &TerminalViewerInput<'_>) -> String {
    match (input.retained_output_bytes, input.output_bytes) {
        (Some(retained), Some(original)) => {
            format!("output truncated · showing {retained} of {original} bytes")
        }
        _ => "output truncated".to_owned(),
    }
}

fn terminal_viewer_chrome_row_count(input: &TerminalViewerInput<'_>, width: u16) -> u16 {
    let mut rows = Vec::new();
    if input.show_status {
        push_wrapped_styled_text(
            &mut rows,
            vec![Span::styled("  ", muted_style())],
            &terminal_status(input),
            width,
            terminal_status_style(input),
            muted_style(),
        );
    }
    if input.output_truncated {
        push_wrapped_styled_text(
            &mut rows,
            vec![Span::styled("  ", muted_style())],
            &terminal_truncation_status(input),
            width,
            muted_style(),
            muted_style(),
        );
    }
    u16::try_from(rows.len()).unwrap_or(u16::MAX)
}

fn terminal_output_lines(input: &TerminalViewerInput<'_>) -> Vec<Line> {
    let Ok(mut stream) = TerminalGridStream::new(
        input.columns.max(1),
        input.rows.max(1),
        GridLimits {
            scrollback_rows: MAX_INLINE_TERMINAL_ROWS.saturating_mul(8),
        },
    ) else {
        return ansi_to_lines(input.output);
    };
    stream.process(input.output.as_bytes());
    let grid = stream.grid();
    let max_rows = match input.sizing {
        TerminalViewerSizing::Compact => MAX_INLINE_TERMINAL_ROWS,
        TerminalViewerSizing::Live { max_rows, .. } => max_rows,
    };
    let rows = grid.main_content_tail_rows(max_rows);
    let mut lines = rows
        .iter()
        .map(|row| terminal_grid_row_to_line(grid, row))
        .collect::<Vec<_>>();
    match input.sizing {
        TerminalViewerSizing::Compact => preview_lines(&lines, max_rows)
            .into_iter()
            .cloned()
            .collect(),
        TerminalViewerSizing::Live {
            visible_rows,
            max_rows,
        } => {
            let target_rows = visible_rows.max(1).min(max_rows);
            if lines.len() > target_rows {
                lines = lines[lines.len().saturating_sub(target_rows)..].to_vec();
            }
            while lines.len() < target_rows {
                lines.push(Line::default());
            }
            lines
        }
    }
}

fn terminal_viewer_content_row_count(input: TerminalViewerInput<'_>, max_rows: usize) -> usize {
    let Ok(mut stream) = TerminalGridStream::new(
        input.columns.max(1),
        input.rows.max(1),
        GridLimits {
            scrollback_rows: max_rows.saturating_mul(8),
        },
    ) else {
        return ansi_to_lines(input.output).len().max(1).min(max_rows);
    };
    stream.process(input.output.as_bytes());
    stream.grid().main_content_tail_rows(max_rows).len().max(1)
}

fn terminal_grid_row_to_line(grid: &TerminalGrid, row: &PhysicalRow) -> Line {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();
    for cell in row.cells() {
        if cell.is_wide_continuation() {
            continue;
        }
        let style = terminal_grid_style(grid.palette().get(cell.style()));
        if current_style == Some(style) {
            current_text.push_str(cell.text());
            continue;
        }
        if !current_text.is_empty() {
            spans.push(Span::styled(
                current_text,
                current_style.unwrap_or_default(),
            ));
            current_text = String::new();
        }
        current_style = Some(style);
        current_text.push_str(cell.text());
    }
    if !current_text.is_empty() {
        spans.push(Span::styled(
            current_text,
            current_style.unwrap_or_default(),
        ));
    }
    Line::from_spans(spans)
}

const fn terminal_grid_style(style: GridStyle) -> Style {
    let mut output = Style::new();
    if let Some(fg) = style.fg {
        output = output.fg(terminal_grid_color(fg));
    }
    if let Some(bg) = style.bg {
        output = output.bg(terminal_grid_color(bg));
    }
    if style.bold {
        output = output.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        output = output.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        output = output.add_modifier(Modifier::UNDERLINE);
    }
    if style.dim {
        output = output.add_modifier(Modifier::DIM);
    }
    if style.inverse {
        output = output.add_modifier(Modifier::REVERSED);
    }
    if style.strike {
        output = output.add_modifier(Modifier::CROSSED_OUT);
    }
    output
}

const fn terminal_grid_color(color: GridColor) -> Color {
    match color {
        GridColor::Indexed(index) => ansi_indexed_color(index),
        GridColor::Rgb { r, g, b } => Color::Rgb(r, g, b),
    }
}

const fn ansi_indexed_color(index: u8) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        8 => Color::BrightBlack,
        9 => Color::BrightRed,
        10 => Color::BrightGreen,
        11 => Color::BrightYellow,
        12 => Color::BrightBlue,
        13 => Color::BrightMagenta,
        14 => Color::BrightCyan,
        15 => Color::BrightWhite,
        other => Color::Indexed(other),
    }
}

fn preview_lines<T>(lines: &[T], max_rows: usize) -> Vec<&T> {
    lines.iter().take(max_rows).collect()
}

fn prefix_line(mut line: Line, prefix: &str, style: Style) -> Line {
    line.spans.insert(0, Span::styled(prefix.to_owned(), style));
    line
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

fn push_wrapped_styled_text(
    rows: &mut Vec<Line>,
    prefix: Vec<Span>,
    text: &str,
    width: u16,
    first_style: Style,
    rest_style: Style,
) {
    let available = usize::from(width)
        .saturating_sub(line_display_width(&prefix))
        .max(1);
    let mut first = true;
    for source_line in text.lines() {
        let wrapped = wrap_text(source_line, available);
        for segment in wrapped {
            let mut spans = if first {
                prefix.clone()
            } else {
                vec![Span::styled(
                    " ".repeat(line_display_width(&prefix)),
                    rest_style,
                )]
            };
            spans.push(Span::styled(
                segment,
                if first { first_style } else { rest_style },
            ));
            rows.push(Line::from_spans(spans));
            first = false;
        }
    }
    if text.is_empty() {
        rows.push(Line::from_spans(prefix));
    }
}

fn line_display_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        if current_width >= width {
            rows.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(1);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;

    fn rendered_text(rows: &[Line]) -> String {
        rows.iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn terminal_content_colors_remain_content_owned() {
        let rows = terminal_viewer_rows(
            TerminalViewerInput {
                output: "\u{1b}[38;2;12;34;56mRGB\u{1b}[0m \u{1b}[31mANSI\u{1b}[0m plain\n",
                columns: 80,
                rows: 24,
                exit_code: Some(0),
                timed_out: Some(false),
                elapsed: None,
                output_truncated: false,
                output_bytes: None,
                retained_output_bytes: None,
                show_status: true,
                sizing: TerminalViewerSizing::Compact,
            },
            100,
        );
        let output_spans = rows
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| matches!(span.content.as_str(), "RGB" | "ANSI" | "plain"))
            .collect::<Vec<_>>();

        assert!(
            output_spans
                .iter()
                .any(|span| span.content == "RGB" && span.style.fg == Some(Color::Rgb(12, 34, 56)))
        );
        assert!(
            output_spans
                .iter()
                .any(|span| span.content == "ANSI" && span.style.fg == Some(Color::Red))
        );
        let rendered = rendered_text(&rows);
        assert!(rendered.contains("plain"));
        assert_eq!(rows[0].spans[0].style, muted_style());
    }

    #[test]
    fn terminal_viewer_interprets_carriage_return() {
        let rows = terminal_viewer_rows(
            TerminalViewerInput {
                output: "first\rsecond\n",
                columns: 80,
                rows: 24,
                exit_code: Some(0),
                timed_out: Some(false),
                elapsed: None,
                output_truncated: false,
                output_bytes: Some(13),
                retained_output_bytes: Some(13),
                show_status: true,
                sizing: TerminalViewerSizing::Compact,
            },
            100,
        );
        let rendered = rendered_text(&rows);

        assert!(rendered.contains("second"), "{rendered}");
        assert!(!rendered.contains("first"), "{rendered}");
    }

    #[test]
    fn live_terminal_state_grows_but_does_not_shrink() {
        let mut state = TerminalViewerLiveState::default();
        let one_line = TerminalViewerInput {
            output: "one\n",
            columns: 80,
            rows: 24,
            exit_code: None,
            timed_out: None,
            elapsed: None,
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            show_status: false,
            sizing: TerminalViewerSizing::Compact,
        };
        state.update(one_line, 28);
        assert_eq!(state.visible_rows(), 1);

        let three_lines = TerminalViewerInput {
            output: "one\ntwo\nthree\n",
            ..one_line
        };
        state.update(three_lines, 28);
        assert_eq!(state.visible_rows(), 3);

        state.update(one_line, 28);
        assert_eq!(state.visible_rows(), 3);
    }

    #[test]
    fn large_output_emits_only_the_configured_tail_bound() {
        use std::fmt::Write as _;
        let output = (0..100_000).fold(String::new(), |mut output, index| {
            let _ = writeln!(output, "line-{index}");
            output
        });
        let rows = terminal_viewer_rows(
            TerminalViewerInput {
                output: &output,
                columns: 80,
                rows: 24,
                exit_code: None,
                timed_out: None,
                elapsed: None,
                output_truncated: false,
                output_bytes: Some(output.len() as u64),
                retained_output_bytes: Some(output.len() as u64),
                show_status: false,
                sizing: TerminalViewerSizing::Compact,
            },
            80,
        );

        assert!(rows.len() <= MAX_INLINE_TERMINAL_ROWS);
        assert!(rendered_text(&rows).contains("line-99999"));
    }

    #[test]
    fn live_terminal_rows_pad_to_reserved_height() {
        let rows = terminal_viewer_rows(
            TerminalViewerInput {
                output: "one\n",
                columns: 80,
                rows: 24,
                exit_code: None,
                timed_out: None,
                elapsed: None,
                output_truncated: false,
                output_bytes: None,
                retained_output_bytes: None,
                show_status: false,
                sizing: TerminalViewerSizing::Live {
                    visible_rows: 3,
                    max_rows: 28,
                },
            },
            100,
        );

        assert_eq!(rows.len(), 3);
        assert!(rendered_text(&rows).contains("one"));
    }

    #[test]
    fn terminal_selection_can_explicitly_delegate_to_parent() {
        let input = TerminalViewerInput {
            output: "text\n",
            columns: 80,
            rows: 24,
            exit_code: None,
            timed_out: None,
            elapsed: None,
            show_status: false,
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            sizing: TerminalViewerSizing::Compact,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 4));
        let mut frame = Frame::new(&mut buffer);
        let state = ComponentSelectionState::new("terminal").parent("transcript");
        let policy = ComponentSelectionPolicy {
            content_capture: SelectionCapture::Delegate,
            ..ComponentSelectionPolicy::content()
        };

        register_terminal_viewer_selection(
            input,
            Rect::new(0, 0, 40, 4),
            &state,
            &policy,
            &mut PaintCx::new(&mut frame),
        );

        let scope = &frame.selection().scopes()[0];
        assert_eq!(scope.capture, SelectionCapture::Delegate);
        assert_eq!(
            scope
                .parent
                .as_ref()
                .map(bmux_tui::selection::SelectionScopeId::as_str),
            Some("transcript")
        );
    }

    #[test]
    fn selection_is_isolated_and_maps_decoded_grid_text() {
        let input = TerminalViewerInput {
            output: "first\rsecond\nwide界\n",
            columns: 80,
            rows: 24,
            exit_code: Some(0),
            timed_out: Some(false),
            elapsed: None,
            show_status: true,
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            sizing: TerminalViewerSizing::Compact,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 8));
        let mut frame = Frame::new(&mut buffer);
        let state = ComponentSelectionState::new("terminal").parent("transcript");

        let outcome = register_terminal_viewer_selection(
            input,
            Rect::new(0, 0, 40, 8),
            &state,
            &ComponentSelectionPolicy::content(),
            &mut PaintCx::new(&mut frame),
        );

        assert!(matches!(
            outcome,
            ComponentSelectionOutcome::ContentRegistered { .. }
        ));
        let scope = &frame.selection().scopes()[0];
        assert_eq!(scope.capture, SelectionCapture::Capture);
        assert_eq!(scope.initiation_area.y, 1);
        let fragments = frame.selection().fragments();
        assert!(
            fragments
                .iter()
                .all(|fragment| fragment.scope_id.as_str() == "terminal")
        );
        assert!(
            fragments
                .iter()
                .all(|fragment| fragment.content_id.as_str() == "terminal.grid")
        );
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment.source_range == (0..1))
        );
        let decoded_len = terminal_output_lines(&input)
            .iter()
            .map(|line| line.plain_text().len().saturating_add(1))
            .sum::<usize>();
        assert!(
            fragments
                .iter()
                .all(|fragment| fragment.source_range.end <= decoded_len)
        );
    }
}
