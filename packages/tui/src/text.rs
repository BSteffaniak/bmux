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
struct WordSegment<'a> {
    /// Styled source slices, or owned pieces supplied by tests.
    pieces: Vec<(std::borrow::Cow<'a, str>, Style)>,
    /// Total display width of this segment.
    width: usize,
    /// Whether this segment is whitespace.
    whitespace: bool,
}

impl WordSegment<'_> {
    const fn new(whitespace: bool) -> Self {
        Self {
            pieces: Vec::new(),
            width: 0,
            whitespace,
        }
    }

    #[cfg(test)]
    fn push(&mut self, grapheme: &str, style: Style) {
        self.width = self.width.saturating_add(display_width(grapheme));
        if let Some((content, last_style)) = self.pieces.last_mut()
            && *last_style == style
        {
            content.to_mut().push_str(grapheme);
            return;
        }
        self.pieces.push((grapheme.to_owned().into(), style));
    }
}

/// Yield whitespace and non-whitespace segments across all spans, retaining
/// only the current segment rather than materializing the entire source line.
fn word_segments(line: &Line) -> impl Iterator<Item = WordSegment<'_>> + '_ {
    let mut graphemes = line
        .spans
        .iter()
        .enumerate()
        .flat_map(|(index, span)| {
            span.content
                .grapheme_indices(true)
                .map(move |(offset, grapheme)| {
                    (
                        index,
                        offset,
                        grapheme,
                        grapheme.chars().all(char::is_whitespace),
                    )
                })
        })
        .peekable();
    std::iter::from_fn(move || {
        let &(first_span, first_offset, _, whitespace) = graphemes.peek()?;
        let mut segment = WordSegment::new(whitespace);
        let mut piece_span = first_span;
        let mut start = first_offset;
        let mut end = start;
        while let Some((index, offset, grapheme, _)) =
            graphemes.next_if(|(_, _, _, is_whitespace)| *is_whitespace == whitespace)
        {
            if index != piece_span {
                let span = &line.spans[piece_span];
                segment.pieces.push((
                    std::borrow::Cow::Borrowed(&span.content[start..end]),
                    span.style,
                ));
                piece_span = index;
                start = offset;
            }
            end = offset + grapheme.len();
            segment.width = segment.width.saturating_add(display_width(grapheme));
        }
        let span = &line.spans[piece_span];
        segment.pieces.push((
            std::borrow::Cow::Borrowed(&span.content[start..end]),
            span.style,
        ));
        Some(segment)
    })
}

/// Wrapping accumulator shared by every policy.
struct WrapSink {
    lines: Vec<Line>,
    column: usize,
    geometry: TextWrapGeometry,
    emitted_rows: usize,
    measure_only: bool,
    pending_rows: usize,
}

impl WrapSink {
    #[cfg(test)]
    fn new(geometry: TextWrapGeometry) -> Self {
        Self::with_mode(geometry, false)
    }

    fn with_mode(geometry: TextWrapGeometry, measure_only: bool) -> Self {
        Self {
            lines: if measure_only {
                Vec::new()
            } else {
                vec![Line::new()]
            },
            column: 0,
            geometry,
            emitted_rows: 0,
            measure_only,
            pending_rows: 1,
        }
    }

    const fn current_width(&self) -> usize {
        self.geometry.width_for_row(
            self.emitted_rows
                .saturating_add(self.pending_rows.saturating_sub(1)),
        )
    }

    fn break_row(&mut self) {
        self.pending_rows = self.pending_rows.saturating_add(1);
        if !self.measure_only {
            self.lines.push(Line::new());
        }
        self.column = 0;
    }

    fn take_row(&mut self) -> Option<Line> {
        if self.pending_rows == 0 {
            return None;
        }
        self.pending_rows -= 1;
        self.emitted_rows = self.emitted_rows.saturating_add(1);
        Some(if self.measure_only {
            Line::new()
        } else {
            self.lines.remove(0)
        })
    }

    fn push_measured_piece(&mut self, content: &str, style: Style, width: usize) {
        if !self.measure_only
            && let Some(last) = self.lines.last_mut()
        {
            if let Some(span) = last.spans.last_mut()
                && span.style == style
            {
                span.content.push_str(content);
            } else {
                last.spans.push(Span::styled(content, style));
            }
        }
        self.column = self.column.saturating_add(width);
    }

    /// Emit graphemes, breaking whenever the current row is full.
    #[cfg(test)]
    fn push_graphemes(&mut self, content: &str, style: Style) {
        for grapheme in content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if self.column > 0 && self.column.saturating_add(grapheme_width) > self.current_width()
            {
                self.break_row();
            }
            self.push_measured_piece(grapheme, style, grapheme_width);
        }
    }

    fn push_segment(&mut self, segment: WordSegment<'_>) {
        if self.measure_only {
            self.column = self.column.saturating_add(segment.width);
            return;
        }
        for (content, style) in segment.pieces {
            if let Some(last) = self.lines.last_mut() {
                if let Some(span) = last.spans.last_mut()
                    && span.style == style
                {
                    span.content.push_str(&content);
                } else {
                    last.spans.push(Span::styled(content.into_owned(), style));
                }
            }
        }
        self.column = self.column.saturating_add(segment.width);
    }

    #[cfg(test)]
    fn finish(self) -> Vec<Line> {
        self.lines
    }
}

/// Count character-wrapped rows without allocating rendered spans or strings.
#[cfg(test)]
pub(crate) fn character_row_count(line: &Line, geometry: TextWrapGeometry) -> usize {
    character_row_count_up_to(line, geometry, usize::MAX)
}

/// Count at most `limit` rows, stopping traversal once that bound is reached.
#[cfg(test)]
pub(crate) fn character_row_count_up_to(
    line: &Line,
    geometry: TextWrapGeometry,
    limit: usize,
) -> usize {
    character_rows_fold(
        line.spans.iter().flat_map(|span| {
            span.content
                .graphemes(true)
                .map(move |text| (text, span.style))
        }),
        geometry,
        || (),
        |(), _, _| {},
    )
    .take(limit)
    .count()
}

/// Produce character-wrapped rows on demand, retaining only the current row.
pub(crate) fn character_rows(
    line: &Line,
    geometry: TextWrapGeometry,
) -> impl Iterator<Item = Line> + '_ {
    let graphemes = line.spans.iter().flat_map(|span| {
        span.content
            .graphemes(true)
            .map(move |text| (text, span.style))
    });
    character_rows_from(graphemes, geometry)
}

fn character_rows_from<'a>(
    graphemes: impl Iterator<Item = (&'a str, Style)> + 'a,
    geometry: TextWrapGeometry,
) -> impl Iterator<Item = Line> + 'a {
    character_rows_fold(graphemes, geometry, Line::new, |row, text, style| {
        if let Some(span) = row.spans.last_mut()
            && span.style == style
        {
            span.content.push_str(text);
        } else {
            row.spans.push(Span::styled(text, style));
        }
    })
}

fn character_rows_fold<'a, R>(
    graphemes: impl Iterator<Item = (&'a str, Style)> + 'a,
    geometry: TextWrapGeometry,
    mut new_row: impl FnMut() -> R + 'a,
    mut append: impl FnMut(&mut R, &str, Style) + 'a,
) -> impl Iterator<Item = R> + 'a {
    let mut graphemes = graphemes
        .map(|(text, style)| (text, style, display_width(text)))
        .peekable();
    let mut row_index = 0usize;
    let mut finished = false;
    std::iter::from_fn(move || {
        if finished {
            return None;
        }
        let mut row = new_row();
        let mut column = 0usize;
        let width = geometry.width_for_row(row_index);
        while let Some(&(text, style, text_width)) = graphemes.peek() {
            if column > 0 && column.saturating_add(text_width) > width {
                break;
            }
            graphemes.next();
            append(&mut row, text, style);
            column = column.saturating_add(text_width);
        }
        finished = graphemes.peek().is_none();
        row_index = row_index.saturating_add(1);
        Some(row)
    })
}

/// Yield completed word-wrapped rows without retaining preceding output.
/// Source segments are buffered, but oversized segments produce rows on demand.
pub(crate) fn word_rows(
    line: &Line,
    geometry: TextWrapGeometry,
) -> impl Iterator<Item = Line> + '_ {
    word_rows_from_segments(word_segments(line), geometry)
}

/// Count word-wrapped rows without constructing rendered text.
#[cfg(test)]
pub(crate) fn word_row_count(line: &Line, geometry: TextWrapGeometry) -> usize {
    word_row_count_up_to(line, geometry, usize::MAX)
}

/// Count at most `limit` rows without constructing rendered text.
#[cfg(test)]
pub(crate) fn word_row_count_up_to(line: &Line, geometry: TextWrapGeometry, limit: usize) -> usize {
    word_rows_with_mode(word_segments(line), geometry, true)
        .take(limit)
        .count()
}

fn word_rows_from_segments<'a>(
    segments: impl Iterator<Item = WordSegment<'a>>,
    geometry: TextWrapGeometry,
) -> impl Iterator<Item = Line> {
    word_rows_with_mode(segments, geometry, false)
}

fn word_rows_with_mode<'a>(
    mut segments: impl Iterator<Item = WordSegment<'a>>,
    geometry: TextWrapGeometry,
    measure_only: bool,
) -> impl Iterator<Item = Line> {
    let mut sink = WrapSink::with_mode(geometry, measure_only);
    let mut pending: Option<(WordSegment<'a>, usize, usize)> = None;
    let mut fitting = None;
    let mut boundary_grapheme = None;
    let mut wrapped_any = false;
    let mut finished = false;
    std::iter::from_fn(move || {
        loop {
            if sink.pending_rows > 1 {
                return sink.take_row();
            }
            if let Some(segment) = fitting.take() {
                sink.push_segment(segment);
            }
            if let Some((segment, piece, offset)) = pending.as_mut() {
                if let Some((content, style)) = segment.pieces.get(*piece) {
                    let measured = boundary_grapheme.take().or_else(|| {
                        content[*offset..]
                            .graphemes(true)
                            .next()
                            .map(|grapheme| (grapheme.len(), display_width(grapheme)))
                    });
                    if let Some((length, width)) = measured {
                        if sink.column > 0
                            && sink.column.saturating_add(width) > sink.current_width()
                        {
                            boundary_grapheme = Some((length, width));
                            sink.break_row();
                            continue;
                        }
                        sink.push_measured_piece(
                            &content[*offset..*offset + length],
                            *style,
                            width,
                        );
                        *offset += length;
                    } else {
                        *piece += 1;
                        *offset = 0;
                    }
                    continue;
                }
                pending = None;
            }
            if finished {
                return None;
            }
            let Some(segment) = segments.next() else {
                finished = true;
                return sink.take_row();
            };
            // Preserve source indentation, but discard whitespace consumed by wrapping.
            if segment.whitespace && sink.column == 0 && wrapped_any {
                continue;
            }
            let breaks =
                sink.column > 0 && sink.column.saturating_add(segment.width) > sink.current_width();
            if breaks {
                sink.break_row();
                wrapped_any = true;
            }
            if !(breaks && segment.whitespace) {
                if segment.width > sink.current_width() {
                    pending = Some((segment, 0, 0));
                    wrapped_any = true;
                } else if breaks {
                    // Yield the completed row before allocating the next row's spans.
                    fitting = Some(segment);
                } else {
                    sink.push_segment(segment);
                }
            }
        }
    })
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
        TextWrap::Character => character_rows(line, geometry).collect(),
        TextWrap::Word => word_rows(line, geometry).collect(),
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
    fn word_segments_preserve_unicode_and_styles_across_empty_spans() {
        let green = Style::new().fg(Color::Green);
        let line = Line::from_spans(vec![
            Span::raw(""),
            Span::raw("he"),
            Span::styled("llo", green),
            Span::raw(""),
            Span::raw("\t\u{2003}"),
            Span::styled("界e\u{301}", green),
        ]);
        let mut segments = super::word_segments(&line);
        let word = segments.next().unwrap();
        assert!(!word.whitespace);
        assert_eq!(word.width, 5);
        assert_eq!(
            word.pieces,
            vec![("he".into(), Style::new()), ("llo".into(), green)]
        );
        let space = segments.next().unwrap();
        assert!(space.whitespace);
        assert_eq!(space.pieces, vec![("\t\u{2003}".into(), Style::new())]);
        let unicode = segments.next().unwrap();
        assert!(!unicode.whitespace);
        assert_eq!(unicode.width, 3);
        assert_eq!(unicode.pieces, vec![("界e\u{301}".into(), green)]);
        assert!(segments.next().is_none());
        assert!(super::word_segments(&Line::new()).next().is_none());
    }

    #[test]
    fn character_wrap_merges_matching_styles_without_crossing_row_boundaries() {
        let green = Style::new().fg(Color::Green);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from_spans(vec![
            Span::styled("ab", green),
            Span::styled("界", green),
            Span::styled("e\u{301}f", blue),
            Span::styled("gh", blue),
        ]);
        assert_eq!(
            line.wrap_character(5),
            vec![
                Line::from_spans(vec![
                    Span::styled("ab界", green),
                    Span::styled("e\u{301}", blue)
                ]),
                Line::from_spans(vec![Span::styled("fgh", blue)]),
            ]
        );
    }

    #[test]
    fn fitting_segment_moves_storage_and_merges_adjacent_style() {
        let style = Style::new().fg(Color::Green);
        let mut segment = super::WordSegment::new(false);
        segment.push("hello", style);
        let storage = segment.pieces[0].0.as_ptr();
        let mut sink = super::WrapSink::new(TextWrapGeometry::uniform(20));
        sink.push_segment(segment);
        assert_eq!(sink.lines[0].spans[0].content.as_ptr(), storage);
        assert_eq!(sink.column, 5);
        let mut space = super::WordSegment::new(true);
        space.push(" ", style);
        sink.push_segment(space);
        assert_eq!(sink.column, 6);
        assert_eq!(
            sink.finish(),
            vec![Line::from_spans(vec![Span::styled("hello ", style)])]
        );
    }

    #[test]
    fn borrowed_segments_merge_into_existing_output_storage() {
        let mut content = String::with_capacity(32);
        content.push_str("prefix ");
        let storage = content.as_ptr();
        let mut sink = super::WrapSink::new(TextWrapGeometry::uniform(32));
        sink.lines[0] = Line::raw(content);
        sink.column = 7;
        let source = Line::raw("hello world");
        for segment in super::word_segments(&source) {
            sink.push_segment(segment);
        }
        assert_eq!(sink.column, 18);
        assert_eq!(sink.lines[0].spans.len(), 1);
        assert_eq!(sink.lines[0].spans[0].content, "prefix hello world");
        assert_eq!(sink.lines[0].spans[0].content.as_ptr(), storage);
    }

    #[test]
    fn bounded_measurement_matches_full_row_counts() {
        for text in ["", "   ", "one two three", "界🙂abcdef\u{301}   last"] {
            let line = Line::raw(text);
            for first in 0..=5 {
                for continuation in 0..=5 {
                    let geometry = TextWrapGeometry::with_continuation(first, continuation);
                    let characters = super::character_row_count(&line, geometry);
                    let words = super::word_row_count(&line, geometry);
                    for limit in (0..=characters.max(words).saturating_add(1)).chain([usize::MAX]) {
                        assert_eq!(
                            super::character_row_count_up_to(&line, geometry, limit),
                            characters.min(limit)
                        );
                        assert_eq!(
                            super::word_row_count_up_to(&line, geometry, limit),
                            words.min(limit)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn word_row_count_matches_rendered_geometry() {
        for text in [
            "",
            "   ",
            "ab   cd",
            "界界 abcdefgh ij",
            "  e\u{301} e\u{301}x",
            "a\u{2003}b",
        ] {
            let line = Line::raw(text);
            for first in 0..=6 {
                for continuation in 0..=6 {
                    let geometry = TextWrapGeometry::with_continuation(first, continuation);
                    assert_eq!(
                        super::word_row_count(&line, geometry),
                        super::word_rows(&line, geometry).count(),
                        "{text:?}: {geometry:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn character_row_count_matches_rendered_geometry() {
        for text in ["", "abc def", "界界a", "e\u{301}x", "\u{200d}", "  a  "] {
            let line = Line::raw(text);
            for first in 0..=5 {
                for continuation in 0..=5 {
                    let geometry = TextWrapGeometry::with_continuation(first, continuation);
                    assert_eq!(
                        super::character_row_count(&line, geometry),
                        super::character_rows(&line, geometry).count(),
                        "{text:?}: {geometry:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn word_segments_classify_entire_unicode_graphemes() {
        let line = Line::raw("a\u{2003}b \u{301}c\t");
        let segments: Vec<_> = super::word_segments(&line)
            .map(|segment| {
                (
                    segment.whitespace,
                    segment
                        .pieces
                        .iter()
                        .map(|(text, _)| text.as_ref())
                        .collect::<String>(),
                )
            })
            .collect();
        assert_eq!(
            segments,
            vec![
                (false, "a".to_string()),
                (true, "\u{2003}".to_string()),
                (false, "b \u{301}c".to_string()),
                (true, "\t".to_string()),
            ]
        );
    }

    #[test]
    fn word_segments_borrow_source_storage() {
        let line = Line::raw("hello world");
        let segments: Vec<_> = super::word_segments(&line).collect();
        for (segment, offset) in segments.iter().zip([0, 5, 6]) {
            assert!(matches!(segment.pieces[0].0, std::borrow::Cow::Borrowed(_)));
            assert_eq!(
                segment.pieces[0].0.as_ptr(),
                line.spans[0].content[offset..].as_ptr()
            );
        }
        assert_eq!(segments.len(), 3);
    }

    #[test]
    fn bounded_character_measurement_stops_after_boundary_lookahead() {
        for limit in 0..=3 {
            let consumed = std::cell::Cell::new(0usize);
            let graphemes = std::iter::repeat_n(("界", Style::new()), 10_000).inspect(|_| {
                consumed.set(consumed.get() + 1);
            });
            let rows = super::character_rows_fold(
                graphemes,
                TextWrapGeometry::uniform(4),
                || (),
                |(), _, _| {},
            );
            assert_eq!(rows.take(limit).count(), limit);
            let expected = if limit == 0 { 0 } else { limit * 2 + 1 };
            assert_eq!(consumed.get(), expected);
        }
    }

    #[test]
    fn measurement_sink_tracks_boundaries_without_row_storage() {
        let mut sink = super::WrapSink::with_mode(TextWrapGeometry::with_continuation(3, 5), true);
        assert_eq!(sink.current_width(), 3);
        sink.push_measured_piece("ab", Style::new(), 2);
        sink.break_row();
        assert_eq!(sink.current_width(), 5);
        assert_eq!(sink.take_row(), Some(Line::new()));
        sink.push_measured_piece("界", Style::new(), 2);
        assert_eq!(sink.column, 2);
        assert_eq!(sink.take_row(), Some(Line::new()));
        assert_eq!(sink.take_row(), None);
        assert_eq!(sink.lines.capacity(), 0);
    }

    #[test]
    fn bounded_word_measurement_emits_no_spans_and_leaves_tail_unread() {
        let line = Line::from_spans(vec![
            Span::styled("ab ", Style::new().fg(Color::Green)),
            Span::styled("abcdefgh", Style::new().fg(Color::Blue)),
            Span::raw(" tail"),
        ]);
        for limit in 0..=3 {
            let consumed = std::cell::Cell::new(0usize);
            let segments = super::word_segments(&line).inspect(|_| {
                consumed.set(consumed.get() + 1);
            });
            let rows = super::word_rows_with_mode(segments, TextWrapGeometry::uniform(3), true);
            let mut count = 0;
            for row in rows.take(limit) {
                assert!(row.spans.is_empty());
                count += 1;
            }
            assert_eq!(count, limit);
            assert_eq!(consumed.get(), if limit == 0 { 0 } else { 3 });
        }
    }

    #[test]
    fn word_rows_consume_segments_only_as_output_is_requested() {
        let consumed = std::cell::Cell::new(0usize);
        let line = Line::raw("ab abcdefgh tail");
        let segments = super::word_segments(&line).inspect(|_| {
            consumed.set(consumed.get() + 1);
        });
        let mut rows = super::word_rows_from_segments(segments, TextWrapGeometry::uniform(3));
        assert_eq!(consumed.get(), 0);
        assert_eq!(rows.next(), Some(Line::raw("ab ")));
        assert_eq!(consumed.get(), 3);
        assert_eq!(rows.next(), Some(Line::raw("abc")));
        assert_eq!(consumed.get(), 3);
        assert_eq!(rows.next(), Some(Line::raw("def")));
        assert_eq!(consumed.get(), 3);
        assert_eq!(rows.next(), Some(Line::raw("gh ")));
        assert_eq!(consumed.get(), 5);
        assert_eq!(rows.next(), Some(Line::raw("tai")));
        assert_eq!(consumed.get(), 5);
        assert_eq!(rows.next(), Some(Line::raw("l")));
        assert_eq!(rows.next(), None);
    }

    #[test]
    fn oversized_word_boundary_preserves_wide_and_combining_graphemes() {
        let green = Style::new().fg(Color::Green);
        let line = Line::from_spans(vec![Span::raw("a"), Span::styled("界e\u{301}🙂", green)]);
        let geometry = TextWrapGeometry::with_continuation(1, 2);
        let expected = vec![
            Line::raw("a"),
            Line::from_spans(vec![Span::styled("界", green)]),
            Line::from_spans(vec![Span::styled("e\u{301}", green)]),
            Line::from_spans(vec![Span::styled("🙂", green)]),
        ];
        assert_eq!(
            super::word_rows(&line, geometry).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(super::word_row_count(&line, geometry), expected.len());
    }

    #[test]
    fn word_rows_resume_fitting_segment_after_yield() {
        let green = Style::new().fg(Color::Green);
        let line = Line::from_spans(vec![Span::raw("ab "), Span::styled("cde", green)]);
        for measure_only in [false, true] {
            let mut rows = super::word_rows_with_mode(
                super::word_segments(&line),
                TextWrapGeometry::with_continuation(3, 5),
                measure_only,
            );
            let expected = if measure_only {
                Line::new()
            } else {
                Line::raw("ab ")
            };
            assert_eq!(rows.next(), Some(expected));
            let expected = if measure_only {
                Line::new()
            } else {
                Line::from_spans(vec![Span::styled("cde", green)])
            };
            assert_eq!(rows.next(), Some(expected));
            assert_eq!(rows.next(), None);
            assert_eq!(rows.next(), None);
        }
    }

    #[test]
    fn oversized_word_resumes_when_continuation_cannot_fit_wide_graphemes() {
        let green = Style::new().fg(Color::Green);
        let line = Line::from_spans(vec![Span::styled("ab界🙂e\u{301}", green)]);
        for continuation in [0, 1] {
            let geometry = TextWrapGeometry::with_continuation(3, continuation);
            let expected: Vec<_> = ["ab", "界", "🙂", "e\u{301}"]
                .into_iter()
                .map(|text| Line::from_spans(vec![Span::styled(text, green)]))
                .collect();
            assert_eq!(
                super::word_rows(&line, geometry).collect::<Vec<_>>(),
                expected
            );
            assert_eq!(super::word_row_count(&line, geometry), expected.len());
        }
    }

    #[test]
    fn word_rows_resume_inside_styled_oversized_segments() {
        let green = Style::new().fg(Color::Green);
        let line = Line::from_spans(vec![
            Span::raw("a "),
            Span::styled("界界界界", green),
            Span::raw("xy z"),
        ]);
        let mut rows = super::word_rows(&line, TextWrapGeometry::with_continuation(3, 4));
        assert_eq!(rows.next(), Some(Line::raw("a ")));
        assert_eq!(
            rows.next(),
            Some(Line::from_spans(vec![Span::styled("界界", green)]))
        );
        assert_eq!(
            rows.next(),
            Some(Line::from_spans(vec![Span::styled("界界", green)]))
        );
        assert_eq!(rows.next(), Some(Line::raw("xy z")));
        assert_eq!(rows.next(), None);
        assert_eq!(rows.next(), None);
    }

    #[test]
    fn long_styled_word_preserves_unicode_across_variable_row_widths() {
        let green = Style::new().fg(Color::Green);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from_spans(vec![
            Span::styled("界a", green),
            Span::styled("e\u{301}🙂z", blue),
        ]);
        let rows = wrap_line_with_geometry(
            &line,
            TextWrapGeometry::with_continuation(3, 2),
            TextWrap::Word,
        );
        assert_eq!(
            rows,
            vec![
                Line::from_spans(vec![Span::styled("界a", green)]),
                Line::from_spans(vec![Span::styled("e\u{301}", blue)]),
                Line::from_spans(vec![Span::styled("🙂", blue)]),
                Line::from_spans(vec![Span::styled("z", blue)]),
            ]
        );
        assert_eq!(
            rows.iter().map(Line::plain_text).collect::<String>(),
            line.plain_text()
        );
    }

    #[test]
    fn character_rows_consume_only_requested_rows_and_one_lookahead() {
        let consumed = std::cell::Cell::new(0usize);
        let source = std::iter::repeat_n(("a", Style::new()), 100_000).inspect(|_| {
            consumed.set(consumed.get() + 1);
        });
        let mut rows = super::character_rows_from(source, TextWrapGeometry::uniform(4));
        assert_eq!(consumed.get(), 0);
        assert_eq!(rows.next().unwrap().plain_text(), "aaaa");
        assert_eq!(consumed.get(), 5);
        assert_eq!(rows.next().unwrap().plain_text(), "aaaa");
        assert_eq!(consumed.get(), 9);
        drop(rows);
        assert_eq!(consumed.get(), 9);
    }

    #[test]
    fn character_rows_match_accumulator_for_unicode_and_geometry() {
        for source in ["", "abcdef", "界e\u{301}🙂 x", "\u{200b}ab", "   "] {
            let line = Line::from_spans(vec![
                Span::raw(source),
                Span::styled(source, Style::new().fg(Color::Green)),
            ]);
            for first in 0..5 {
                for continuation in 0..5 {
                    let geometry = TextWrapGeometry::with_continuation(first, continuation);
                    let mut expected = super::WrapSink::new(geometry);
                    for span in &line.spans {
                        expected.push_graphemes(&span.content, span.style);
                    }
                    let mut rows = super::character_rows(&line, geometry);
                    assert_eq!(rows.by_ref().collect::<Vec<_>>(), expected.finish());
                    assert!(rows.next().is_none());
                    assert!(rows.next().is_none());
                }
            }
        }
    }

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
