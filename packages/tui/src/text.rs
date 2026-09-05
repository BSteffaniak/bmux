//! Styled text primitives.

use crate::style::Style;
use crate::text_width::display_width;
use unicode_segmentation::UnicodeSegmentation;

/// A styled text span.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    /// Span text.
    pub content: String,
    /// Span style.
    pub style: Style,
}

impl Span {
    /// Create an unstyled span.
    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            style: Style::new(),
        }
    }

    /// Create a styled span.
    #[must_use]
    pub fn styled(content: impl Into<String>, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }
    /// Return a copy of this span with `style` patched over its current style.
    #[must_use]
    pub fn patch_style(&self, style: Style) -> Self {
        Self::styled(self.content.clone(), self.style.patch(style))
    }

    /// Return the terminal display width of this span.
    #[must_use]
    pub fn width(&self) -> usize {
        display_width(&self.content)
    }
}

/// A line of styled text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    /// Ordered spans in this line.
    pub spans: Vec<Span>,
}

impl Line {
    /// Create an empty line.
    #[must_use]
    pub const fn new() -> Self {
        Self { spans: Vec::new() }
    }

    /// Create a line from unstyled text.
    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::raw(content)],
        }
    }

    /// Create a line from spans.
    #[must_use]
    pub fn from_spans(spans: impl Into<Vec<Span>>) -> Self {
        Self {
            spans: spans.into(),
        }
    }

    /// Return a copy of this line with `style` applied behind each span's
    /// explicit style.
    ///
    /// This is useful when rendering text on an opaque surface: callers can
    /// supply the surface style as a fallback while preserving span-specific
    /// foreground colors and modifiers.
    #[must_use]
    pub fn with_fallback_style(&self, style: Style) -> Self {
        Self::from_spans(
            self.spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), style.patch(span.style)))
                .collect::<Vec<_>>(),
        )
    }

    /// Return a copy of this line with `style` patched over each span.
    #[must_use]
    pub fn patch_style(&self, style: Style) -> Self {
        Self::from_spans(
            self.spans
                .iter()
                .map(|span| span.patch_style(style))
                .collect::<Vec<_>>(),
        )
    }

    /// Return the terminal display width of this line.
    #[must_use]
    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    /// Return a copy truncated to terminal display width with an ellipsis when clipped.
    #[must_use]
    pub fn truncate(&self, width: usize) -> Self {
        truncate_line_to_display_width(self, width)
    }

    /// Return a styled viewport clipped to terminal display cells.
    ///
    /// Graphemes that would be split by the left or right viewport edge are
    /// omitted, preserving valid terminal cell alignment and span styles.
    #[must_use]
    pub fn viewport(&self, horizontal_offset: usize, width: usize) -> Self {
        line_viewport(self, horizontal_offset, width)
    }

    /// Return this line wrapped at grapheme boundaries.
    #[must_use]
    pub fn wrap_character(&self, width: usize) -> Vec<Self> {
        wrap_line_character(self, width)
    }

    /// Return this line wrapped at word boundaries when possible.
    #[must_use]
    pub fn wrap_word(&self, width: usize) -> Vec<Self> {
        wrap_line_word(self, width)
    }

    /// Return this line wrapped using an explicit policy and per-row geometry.
    #[must_use]
    pub fn wrap(&self, geometry: TextWrapGeometry, wrap: TextWrap) -> Vec<Self> {
        wrap_line_with_geometry(self, geometry, wrap)
    }

    /// Append a span to the line.
    pub fn push_span(&mut self, span: Span) {
        self.spans.push(span);
    }

    /// Return the plain text for this line.
    #[must_use]
    pub fn plain_text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.content.as_str())
            .collect()
    }
}

/// Return a copy of a styled line truncated to terminal display width.
///
/// The ellipsis inherits the style of the first clipped grapheme, or the final
/// visible span when clipping happens at the end of the line.
#[must_use]
pub fn truncate_line_to_display_width(line: &Line, width: usize) -> Line {
    if line.width() <= width {
        return line.clone();
    }
    if width == 0 {
        return Line::new();
    }
    if width == 1 {
        let style = line
            .spans
            .iter()
            .find(|span| !span.content.is_empty())
            .map_or_else(Style::new, |span| span.style);
        return Line::from_spans([Span::styled("…", style)]);
    }

    let body_width = width.saturating_sub(1);
    let mut used = 0usize;
    let mut spans: Vec<Span> = Vec::new();
    let mut ellipsis_style = None;
    'outer: for span in &line.spans {
        let mut content = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if grapheme_width == 0 {
                continue;
            }
            if used.saturating_add(grapheme_width) > body_width {
                if !content.is_empty() {
                    push_or_merge_span(&mut spans, content, span.style);
                }
                ellipsis_style = Some(span.style);
                break 'outer;
            }
            content.push_str(grapheme);
            used = used.saturating_add(grapheme_width);
        }
        if !content.is_empty() {
            push_or_merge_span(&mut spans, content, span.style);
        }
    }
    let style = ellipsis_style
        .or_else(|| spans.last().map(|span| span.style))
        .unwrap_or_else(Style::new);
    push_or_merge_span(&mut spans, "…".to_owned(), style);
    Line::from_spans(spans)
}

/// Return a styled viewport clipped to terminal display cells.
///
/// This helper preserves span styles and never splits a Unicode grapheme. When
/// the viewport begins or ends inside a wide grapheme, that grapheme is omitted
/// so subsequent cells remain aligned.
#[must_use]
pub fn line_viewport(line: &Line, horizontal_offset: usize, width: usize) -> Line {
    if width == 0 {
        return Line::new();
    }
    if horizontal_offset == 0 && line.width() <= width {
        return line.clone();
    }

    let start = horizontal_offset;
    let end = start.saturating_add(width);
    let mut cursor = 0usize;
    let mut spans: Vec<Span> = Vec::new();
    for span in &line.spans {
        let mut content = String::new();
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if grapheme_width == 0 {
                continue;
            }
            let next = cursor.saturating_add(grapheme_width);
            if next <= start || cursor < start {
                cursor = next;
                continue;
            }
            if cursor >= end || next > end {
                break;
            }
            content.push_str(grapheme);
            cursor = next;
        }
        if !content.is_empty() {
            push_or_merge_span(&mut spans, content, span.style);
        }
        if cursor >= end {
            break;
        }
    }
    Line::from_spans(spans)
}

fn push_or_merge_span(spans: &mut Vec<Span>, content: String, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.push_str(&content);
        return;
    }
    spans.push(Span::styled(content, style));
}

/// Text wrapping policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextWrap {
    /// Do not wrap lines; rendering clips to the target area.
    #[default]
    None,
    /// Wrap at grapheme boundaries when a line exceeds the target width.
    Character,
    /// Wrap at word boundaries when possible, falling back to grapheme wrapping
    /// for words longer than the target width.
    ///
    /// Word detection spans style boundaries, so a word split across differently
    /// styled spans is kept intact when it fits.
    Word,
}

/// Per-row target widths for a wrapping operation.
///
/// A distinct first-row width supports callers that reserve leading space for a
/// label, marker, or prefix and indent continuation rows differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextWrapGeometry {
    /// Target display width for the first produced row.
    pub first_width: usize,
    /// Target display width for every subsequent row.
    pub continuation_width: usize,
}

impl TextWrapGeometry {
    /// Create geometry using one uniform width for every row.
    #[must_use]
    pub const fn uniform(width: usize) -> Self {
        Self {
            first_width: width,
            continuation_width: width,
        }
    }

    /// Create geometry with a distinct first-row width.
    #[must_use]
    pub const fn with_continuation(first_width: usize, continuation_width: usize) -> Self {
        Self {
            first_width,
            continuation_width,
        }
    }

    /// Return the target width for `row`, clamped to at least one cell.
    #[must_use]
    pub const fn width_for_row(self, row: usize) -> usize {
        let width = if row == 0 {
            self.first_width
        } else {
            self.continuation_width
        };
        if width == 0 { 1 } else { width }
    }
}

impl From<usize> for TextWrapGeometry {
    fn from(width: usize) -> Self {
        Self::uniform(width)
    }
}

/// One contiguous run of graphemes sharing whitespace classification.
///
/// Segments are collected across span boundaries so word wrapping can keep a
/// word intact even when its graphemes carry different styles.
struct WordSegment {
    /// Styled grapheme pieces in source order.
    pieces: Vec<(String, Style)>,
    /// Total display width of this segment.
    width: usize,
    /// Whether this segment is whitespace.
    whitespace: bool,
}

impl WordSegment {
    const fn new(whitespace: bool) -> Self {
        Self {
            pieces: Vec::new(),
            width: 0,
            whitespace,
        }
    }

    fn push(&mut self, grapheme: &str, style: Style) {
        self.width = self.width.saturating_add(display_width(grapheme));
        if let Some((content, last_style)) = self.pieces.last_mut()
            && *last_style == style
        {
            content.push_str(grapheme);
            return;
        }
        self.pieces.push((grapheme.to_owned(), style));
    }

    const fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }
}

/// Split a line into whitespace and non-whitespace segments across all spans.
fn word_segments(line: &Line) -> Vec<WordSegment> {
    let mut segments: Vec<WordSegment> = Vec::new();
    let mut current: Option<WordSegment> = None;

    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let whitespace = !grapheme.is_empty() && grapheme.chars().all(char::is_whitespace);
            match &mut current {
                Some(segment) if segment.whitespace == whitespace => {
                    segment.push(grapheme, span.style);
                }
                Some(_) => {
                    if let Some(finished) = current.take() {
                        segments.push(finished);
                    }
                    let mut segment = WordSegment::new(whitespace);
                    segment.push(grapheme, span.style);
                    current = Some(segment);
                }
                None => {
                    let mut segment = WordSegment::new(whitespace);
                    segment.push(grapheme, span.style);
                    current = Some(segment);
                }
            }
        }
    }
    if let Some(finished) = current {
        segments.push(finished);
    }
    segments
}

/// Wrapping accumulator shared by every policy.
struct WrapSink {
    lines: Vec<Line>,
    column: usize,
    geometry: TextWrapGeometry,
}

impl WrapSink {
    fn new(geometry: TextWrapGeometry) -> Self {
        Self {
            lines: vec![Line::new()],
            column: 0,
            geometry,
        }
    }

    const fn current_width(&self) -> usize {
        self.geometry
            .width_for_row(self.lines.len().saturating_sub(1))
    }

    fn break_row(&mut self) {
        self.lines.push(Line::new());
        self.column = 0;
    }

    fn push_piece(&mut self, content: &str, style: Style) {
        if let Some(last) = self.lines.last_mut() {
            push_or_merge_span(&mut last.spans, content.to_owned(), style);
        }
        self.column = self.column.saturating_add(display_width(content));
    }

    /// Emit graphemes, breaking whenever the current row is full.
    fn push_graphemes(&mut self, content: &str, style: Style) {
        for grapheme in content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if self.column > 0 && self.column.saturating_add(grapheme_width) > self.current_width()
            {
                self.break_row();
            }
            self.push_piece(grapheme, style);
        }
    }

    fn finish(self) -> Vec<Line> {
        self.lines
    }
}

/// Return a styled line wrapped at grapheme boundaries.
#[must_use]
pub fn wrap_line_character(line: &Line, width: usize) -> Vec<Line> {
    wrap_line_with_geometry(line, TextWrapGeometry::uniform(width), TextWrap::Character)
}

/// Return a styled line wrapped at word boundaries when possible.
///
/// Words are detected across span boundaries, so inline style changes inside a
/// word do not introduce a break.
#[must_use]
pub fn wrap_line_word(line: &Line, width: usize) -> Vec<Line> {
    wrap_line_with_geometry(line, TextWrapGeometry::uniform(width), TextWrap::Word)
}

/// Wrap one styled line using an explicit policy and per-row geometry.
///
/// `TextWrap::None` returns the line unchanged. Continuation rows never begin
/// with wrapped whitespace, and words wider than the target width fall back to
/// grapheme wrapping.
#[must_use]
pub fn wrap_line_with_geometry(
    line: &Line,
    geometry: TextWrapGeometry,
    wrap: TextWrap,
) -> Vec<Line> {
    match wrap {
        TextWrap::None => vec![line.clone()],
        TextWrap::Character => {
            let mut sink = WrapSink::new(geometry);
            for span in &line.spans {
                sink.push_graphemes(&span.content, span.style);
            }
            sink.finish()
        }
        TextWrap::Word => {
            let mut sink = WrapSink::new(geometry);
            let mut wrapped_any = false;
            for segment in word_segments(line) {
                if segment.is_empty() {
                    continue;
                }
                // Leading indentation is meaningful, so only whitespace that a
                // wrap break consumed is dropped.
                if segment.whitespace && sink.column == 0 && wrapped_any {
                    continue;
                }
                if sink.column > 0
                    && sink.column.saturating_add(segment.width) > sink.current_width()
                {
                    sink.break_row();
                    wrapped_any = true;
                    if segment.whitespace {
                        continue;
                    }
                }
                if segment.width > sink.current_width() {
                    for (content, style) in &segment.pieces {
                        sink.push_graphemes(content, *style);
                    }
                    wrapped_any = true;
                    continue;
                }
                for (content, style) in &segment.pieces {
                    sink.push_piece(content, *style);
                }
            }
            sink.finish()
        }
    }
}

/// Wrap plain text using an explicit policy and per-row geometry.
///
/// Returns one string per produced row. `TextWrap::None` returns `text`
/// unchanged as a single row.
///
/// This operates directly on the string rather than building intermediate
/// styled lines, keeping allocation proportional to the output.
#[must_use]
pub fn wrap_text(text: &str, geometry: TextWrapGeometry, wrap: TextWrap) -> Vec<String> {
    if matches!(wrap, TextWrap::None) {
        return vec![text.to_owned()];
    }

    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut column = 0usize;
    // Byte offset and starting column of the in-progress word in `current`.
    let mut word_start: Option<(usize, usize)> = None;
    let mut wrapped_any = false;

    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        let max_width = geometry.width_for_row(rows.len());
        let whitespace = matches!(wrap, TextWrap::Word)
            && !grapheme.is_empty()
            && grapheme.chars().all(char::is_whitespace);

        if whitespace {
            word_start = None;
            // Leading indentation is meaningful; only whitespace consumed by a
            // wrap break is dropped.
            if column == 0 && wrapped_any {
                continue;
            }
            if column > 0 && column.saturating_add(grapheme_width) > max_width {
                rows.push(std::mem::take(&mut current));
                column = 0;
                wrapped_any = true;
                continue;
            }
            current.push_str(grapheme);
            column = column.saturating_add(grapheme_width);
            continue;
        }

        if matches!(wrap, TextWrap::Word) && word_start.is_none() {
            word_start = Some((current.len(), column));
        }

        if column > 0 && column.saturating_add(grapheme_width) > max_width {
            match word_start {
                // Move the whole word down instead of splitting it.
                Some((offset, start_column)) if start_column > 0 && offset <= current.len() => {
                    let moved = current.split_off(offset);
                    rows.push(std::mem::take(&mut current));
                    current = moved;
                    column = display_width(&current);
                }
                // The word spans the full row width, so break mid-word.
                _ => {
                    rows.push(std::mem::take(&mut current));
                    column = 0;
                }
            }
            wrapped_any = true;
            if matches!(wrap, TextWrap::Word) {
                word_start = Some((0, 0));
            }
        }
        current.push_str(grapheme);
        column = column.saturating_add(grapheme_width);
    }

    rows.push(current);
    rows
}

/// Multiple lines of styled text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Text {
    /// Ordered lines.
    pub lines: Vec<Line>,
}

impl Text {
    /// Create empty text.
    #[must_use]
    pub const fn new() -> Self {
        Self { lines: Vec::new() }
    }

    /// Create unstyled text, splitting LF and CRLF into logical lines.
    ///
    /// Empty input creates one empty line. Blank lines, including a trailing
    /// empty line after a newline, are preserved. Bare carriage returns are not
    /// line separators.
    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        let content = content.into();
        let mut lines = content.split('\n').peekable();
        let mut result = Vec::new();
        while let Some(line) = lines.next() {
            // Strip CR only when it belongs to a CRLF separator.
            let line = if lines.peek().is_some() {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                line
            };
            result.push(Line::raw(line));
        }
        Self { lines: result }
    }

    /// Create text from lines.
    #[must_use]
    pub fn from_lines(lines: impl Into<Vec<Line>>) -> Self {
        Self {
            lines: lines.into(),
        }
    }

    /// Return a copy of this text with `style` patched over each line.
    #[must_use]
    pub fn patch_style(&self, style: Style) -> Self {
        Self::from_lines(
            self.lines
                .iter()
                .map(|line| line.patch_style(style))
                .collect::<Vec<_>>(),
        )
    }

    /// Return the maximum terminal display width of all lines.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lines.iter().map(Line::width).max().unwrap_or(0)
    }

    /// Append a line.
    pub fn push_line(&mut self, line: Line) {
        self.lines.push(line);
    }
}

impl From<&str> for Span {
    fn from(value: &str) -> Self {
        Self::raw(value)
    }
}

impl From<String> for Span {
    fn from(value: String) -> Self {
        Self::raw(value)
    }
}

impl From<&str> for Line {
    fn from(value: &str) -> Self {
        Self::raw(value)
    }
}

impl From<String> for Line {
    fn from(value: String) -> Self {
        Self::raw(value)
    }
}

impl From<&str> for Text {
    fn from(value: &str) -> Self {
        Self::raw(value)
    }
}

impl From<String> for Text {
    fn from(value: String) -> Self {
        Self::raw(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Line, Span, Text, TextWrap, TextWrapGeometry, line_viewport, wrap_line_character,
        wrap_line_with_geometry, wrap_line_word,
    };
    use crate::style::{Color, Style};

    #[test]
    fn line_plain_text_concatenates_span_content() {
        let line = Line::from_spans(vec![
            Span::styled("hello", Style::new().fg(Color::Green)),
            Span::raw(" world"),
        ]);

        assert_eq!(line.plain_text(), "hello world");
    }

    #[test]
    fn line_with_fallback_style_preserves_explicit_fields() {
        let fallback = Style::new().fg(Color::White).bg(Color::Black);
        let explicit = Style::new().fg(Color::Red);
        let line = Line::from_spans(vec![Span::styled("hello", explicit)]);

        let styled = line.with_fallback_style(fallback);

        assert_eq!(styled.spans[0].style, fallback.patch(explicit));
    }

    #[test]
    fn text_primitives_report_unicode_display_widths() {
        let span = Span::raw("a界");
        let line = Line::from_spans([span.clone(), Span::raw("b")]);
        let text = Text::from_lines([line.clone(), Line::from("x")]);

        assert_eq!(span.width(), 3);
        assert_eq!(line.width(), 4);
        assert_eq!(text.width(), 4);
    }

    #[test]
    fn line_truncate_preserves_styles_and_adds_ellipsis() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from_spans([Span::styled("ab", red), Span::styled("界cd", blue)]);
        let truncated = line.truncate(4);

        assert_eq!(truncated.plain_text(), "ab…");
        assert_eq!(truncated.spans[0], Span::styled("ab", red));
        assert_eq!(truncated.spans[1], Span::styled("…", blue));
    }

    #[test]
    fn line_truncate_handles_tiny_widths() {
        let style = Style::new().fg(Color::Red);
        let line = Line::from_spans([Span::styled("abc", style)]);

        assert_eq!(line.truncate(0), Line::new());
        assert_eq!(
            line.truncate(1),
            Line::from_spans([Span::styled("…", style)])
        );
    }

    #[test]
    fn line_truncate_does_not_split_graphemes() {
        let line = Line::from("a👨‍👩‍👧‍👦b");

        assert_eq!(line.truncate(3).plain_text(), "a…");
    }

    #[test]
    fn line_viewport_clips_ascii_and_preserves_styles() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from_spans([Span::styled("ab", red), Span::styled("cde", blue)]);
        let viewport = line_viewport(&line, 1, 3);

        assert_eq!(viewport.plain_text(), "bcd");
        assert_eq!(viewport.spans.len(), 2);
        assert_eq!(viewport.spans[0], Span::styled("b", red));
        assert_eq!(viewport.spans[1], Span::styled("cd", blue));
    }

    #[test]
    fn line_viewport_merges_adjacent_same_style_spans() {
        let style = Style::new().fg(Color::Green);
        let line = Line::from_spans([Span::styled("ab", style), Span::styled("cd", style)]);
        let viewport = line_viewport(&line, 1, 2);

        assert_eq!(viewport.spans, vec![Span::styled("bc", style)]);
    }

    #[test]
    fn line_viewport_does_not_split_combining_graphemes() {
        let line = Line::from("e\u{301}e\u{301}x");

        assert_eq!(line_viewport(&line, 0, 2).plain_text(), "e\u{301}e\u{301}");
        assert_eq!(line_viewport(&line, 1, 1).plain_text(), "e\u{301}");
    }

    #[test]
    fn line_viewport_does_not_split_emoji_zwj_sequences() {
        let family = "👨‍👩‍👧‍👦";
        let line = Line::from(format!("a{family}b"));

        assert_eq!(
            line_viewport(&line, 0, 3).plain_text(),
            format!("a{family}")
        );
        assert_eq!(line_viewport(&line, 2, 2).plain_text(), "b");
        assert_eq!(line_viewport(&line, 3, 1).plain_text(), "b");
    }

    #[test]
    fn line_viewport_omits_wide_graphemes_cut_by_edges() {
        let line = Line::from("a界b");

        assert_eq!(line_viewport(&line, 0, 2).plain_text(), "a");
        assert_eq!(line_viewport(&line, 1, 2).plain_text(), "界");
        assert_eq!(line_viewport(&line, 2, 2).plain_text(), "b");
    }

    #[test]
    fn shared_wrapping_helpers_preserve_styles() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from_spans([Span::styled("one ", red), Span::styled("two", blue)]);
        let wrapped = wrap_line_word(&line, 4);

        assert_eq!(wrapped[0], Line::from_spans([Span::styled("one ", red)]));
        assert_eq!(wrapped[1], Line::from_spans([Span::styled("two", blue)]));
    }

    #[test]
    fn word_wrap_keeps_words_intact_across_style_boundaries() {
        let plain = Style::new();
        let bold = Style::new().fg(Color::Red);
        // "wraps" is split across two spans, as emphasis produces.
        let line = Line::from_spans([
            Span::styled("alpha wrap", plain),
            Span::styled("s", bold),
            Span::styled(" omega", plain),
        ]);

        let wrapped = wrap_line_word(&line, 12);

        assert_eq!(
            wrapped.iter().map(Line::plain_text).collect::<Vec<_>>(),
            ["alpha wraps ", "omega"],
            "a word split across spans must not break at the style boundary"
        );
    }

    #[test]
    fn word_wrap_merges_adjacent_same_style_spans() {
        let line = Line::from_spans([
            Span::raw("alpha"),
            Span::raw(" "),
            Span::raw("beta"),
            Span::raw(" gamma"),
        ]);

        let wrapped = wrap_line_word(&line, 40);

        assert_eq!(wrapped.len(), 1);
        assert_eq!(
            wrapped[0].spans.len(),
            1,
            "identically styled neighbors must coalesce into one span"
        );
        assert_eq!(wrapped[0].plain_text(), "alpha beta gamma");
    }

    #[test]
    fn character_wrap_merges_adjacent_same_style_graphemes() {
        let line = Line::from_spans([Span::raw("abcdefgh")]);

        let wrapped = wrap_line_character(&line, 4);

        assert_eq!(wrapped.len(), 2);
        assert!(
            wrapped.iter().all(|line| line.spans.len() == 1),
            "character wrapping must not emit one span per grapheme"
        );
        assert_eq!(wrapped[0].plain_text(), "abcd");
        assert_eq!(wrapped[1].plain_text(), "efgh");
    }

    #[test]
    fn wrap_geometry_supports_distinct_first_row_width() {
        let line = Line::raw("alpha beta gamma delta");
        let wrapped = wrap_line_with_geometry(
            &line,
            TextWrapGeometry::with_continuation(12, 6),
            TextWrap::Word,
        );

        assert_eq!(
            wrapped.iter().map(Line::plain_text).collect::<Vec<_>>(),
            ["alpha beta ", "gamma ", "delta"]
        );
    }

    #[test]
    fn wrap_none_returns_the_line_unchanged() {
        let line = Line::raw("alpha beta gamma");
        let wrapped = wrap_line_with_geometry(&line, TextWrapGeometry::uniform(4), TextWrap::None);

        assert_eq!(wrapped, vec![line]);
    }

    #[test]
    fn word_wrap_preserves_leading_indentation() {
        // Pretty-printed structured text relies on leading indentation, so only
        // whitespace consumed by a wrap break may be dropped.
        let line = Line::raw("    \"value\": 1");
        let wrapped = wrap_line_word(&line, 40);

        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].plain_text(), "    \"value\": 1");
    }

    #[test]
    fn word_wrap_preserves_wide_graphemes_for_overlong_words() {
        let line = Line::raw("界界界界");
        let wrapped = wrap_line_word(&line, 3);

        assert_eq!(
            wrapped.iter().map(Line::plain_text).collect::<Vec<_>>(),
            ["界", "界", "界", "界"],
            "wide graphemes must never be split across rows"
        );
    }

    #[test]
    fn text_primitives_patch_styles() {
        let line = Line::from_spans([Span::styled("hi", Style::new().fg(Color::Red))]);
        let patched = line.patch_style(Style::new().bg(Color::Blue));

        assert_eq!(
            patched.spans[0].style,
            Style::new().fg(Color::Red).bg(Color::Blue)
        );
    }
}
