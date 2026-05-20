//! Diff/file view primitives.

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::{Color, Modifier, Style};
use crate::text::{Line, Span};
use crate::widget::StatefulWidget;

/// Summary for one changed file in a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFileSummary {
    /// Old path, when known.
    pub old_path: Option<String>,
    /// New path, when known.
    pub new_path: Option<String>,
    /// Number of added lines.
    pub added: u32,
    /// Number of removed lines.
    pub removed: u32,
}

impl DiffFileSummary {
    /// Create a changed-file summary.
    #[must_use]
    pub fn new(path: impl Into<String>, added: u32, removed: u32) -> Self {
        Self {
            old_path: None,
            new_path: Some(path.into()),
            added,
            removed,
        }
    }

    /// Create a renamed changed-file summary.
    #[must_use]
    pub fn renamed(
        old_path: impl Into<String>,
        new_path: impl Into<String>,
        added: u32,
        removed: u32,
    ) -> Self {
        Self {
            old_path: Some(old_path.into()),
            new_path: Some(new_path.into()),
            added,
            removed,
        }
    }

    /// Return the display path.
    #[must_use]
    pub fn display_path(&self) -> String {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (_, Some(new)) => new.clone(),
            (Some(old), None) => old.clone(),
            (None, None) => String::new(),
        }
    }
}

/// Selection state for a changed-file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffFileListState {
    /// Selected file index, if any.
    pub selected: Option<usize>,
    /// First visible file index.
    pub offset: usize,
}

impl DiffFileListState {
    /// Create empty file-list state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected: None,
            offset: 0,
        }
    }

    /// Ensure selected item remains visible.
    pub fn ensure_selected_visible(&mut self, height: u16, file_count: usize) {
        if file_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        let height = usize::from(height.max(1));
        let selected = self.selected.unwrap_or(0).min(file_count - 1);
        self.selected = Some(selected);
        if selected < self.offset {
            self.offset = selected;
        } else if selected >= self.offset.saturating_add(height) {
            self.offset = selected.saturating_add(1).saturating_sub(height);
        }
        self.offset = self.offset.min(file_count.saturating_sub(1));
    }
}

/// Changed-file list widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFileList<'files> {
    files: &'files [DiffFileSummary],
    style: Style,
    selected_style: Style,
}

impl<'files> DiffFileList<'files> {
    /// Create a changed-file list.
    #[must_use]
    pub const fn new(files: &'files [DiffFileSummary]) -> Self {
        Self {
            files,
            style: Style::new(),
            selected_style: Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// Set base style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set selected row style.
    #[must_use]
    pub const fn selected_style(mut self, style: Style) -> Self {
        self.selected_style = style;
        self
    }

    /// Return file count.
    #[must_use]
    pub const fn file_count(&self) -> usize {
        self.files.len()
    }
}

impl StatefulWidget for DiffFileList<'_> {
    type State = DiffFileListState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        state.ensure_selected_visible(area.height, self.files.len());
        for (row, (index, file)) in self
            .files
            .iter()
            .enumerate()
            .skip(state.offset)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            let selected = state.selected == Some(index);
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &render_file_summary(
                    file,
                    if selected {
                        self.style.patch(self.selected_style)
                    } else {
                        self.style
                    },
                ),
            );
        }
    }
}

fn render_file_summary(file: &DiffFileSummary, style: Style) -> Line {
    Line::from_spans(vec![
        Span::styled(file.display_path(), style),
        Span::styled(" ", style),
        Span::styled(format!("+{}", file.added), style.fg(Color::Green)),
        Span::styled(" ", style),
        Span::styled(format!("-{}", file.removed), style.fg(Color::Red)),
    ])
}

/// Semantic kind for one diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// File header or metadata line.
    FileHeader,
    /// Hunk header line.
    HunkHeader,
    /// Unchanged context line.
    Context,
    /// Added line.
    Added,
    /// Removed line.
    Removed,
}

/// One inline span inside a diff line's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffInlineSpan {
    /// Span content.
    pub content: String,
    /// Style patch applied over the line kind style.
    pub style: Style,
}

impl DiffInlineSpan {
    /// Create an inline diff span.
    #[must_use]
    pub fn new(content: impl Into<String>, style: Style) -> Self {
        Self {
            content: content.into(),
            style,
        }
    }
}

/// One logical line in a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Old-file line number, when present.
    pub old_line: Option<u32>,
    /// New-file line number, when present.
    pub new_line: Option<u32>,
    /// Diff line content without prefix/gutter.
    pub content: String,
    /// Semantic line kind.
    pub kind: DiffLineKind,
    /// Optional inline content spans for changed-region highlighting.
    pub inline_spans: Vec<DiffInlineSpan>,
}

impl DiffLine {
    /// Create a diff line.
    #[must_use]
    pub fn new(
        kind: DiffLineKind,
        old_line: Option<u32>,
        new_line: Option<u32>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            old_line,
            new_line,
            content: content.into(),
            kind,
            inline_spans: Vec::new(),
        }
    }

    /// Set inline content spans.
    #[must_use]
    pub fn inline_spans(mut self, spans: Vec<DiffInlineSpan>) -> Self {
        self.inline_spans = spans;
        self
    }

    /// Return true when the line has inline spans.
    #[must_use]
    pub fn has_inline_spans(&self) -> bool {
        !self.inline_spans.is_empty()
    }
}

/// Diff rendering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffViewMode {
    /// Unified diff rendering.
    #[default]
    Unified,
    /// Side-by-side old/new rendering.
    SideBySide,
    /// Side-by-side for wide areas, unified otherwise.
    Responsive,
}

/// Scroll state for [`DiffView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffViewState {
    /// First visible logical diff line.
    pub offset: usize,
}

impl DiffViewState {
    /// Move to the next hunk header, if one exists.
    pub fn next_hunk(&mut self, lines: &[DiffLine]) -> bool {
        let start = self.offset.saturating_add(1);
        if let Some(index) = lines
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, line)| (line.kind == DiffLineKind::HunkHeader).then_some(index))
        {
            self.offset = index;
            return true;
        }
        false
    }

    /// Move to the previous hunk header, if one exists.
    pub fn previous_hunk(&mut self, lines: &[DiffLine]) -> bool {
        let Some(index) = lines
            .iter()
            .enumerate()
            .take(self.offset)
            .rev()
            .find_map(|(index, line)| (line.kind == DiffLineKind::HunkHeader).then_some(index))
        else {
            return false;
        };
        self.offset = index;
        true
    }
}

/// Styles used by [`DiffView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffViewStyles {
    /// File/header style.
    pub file_header: Style,
    /// Hunk header style.
    pub hunk_header: Style,
    /// Context line style.
    pub context: Style,
    /// Added line style.
    pub added: Style,
    /// Removed line style.
    pub removed: Style,
    /// Gutter style.
    pub gutter: Style,
}

impl Default for DiffViewStyles {
    fn default() -> Self {
        Self {
            file_header: Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
            hunk_header: Style::new().fg(Color::Cyan),
            context: Style::new(),
            added: Style::new().fg(Color::Green),
            removed: Style::new().fg(Color::Red),
            gutter: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// One rendered diff row after applying view transforms such as folding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffRenderRow {
    Line(usize),
    Fold { start: usize, count: usize },
}

/// Virtualized diff view widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffView<'lines> {
    lines: &'lines [DiffLine],
    mode: DiffViewMode,
    styles: DiffViewStyles,
    fold_context_threshold: Option<usize>,
    fold_context_keep: usize,
}

impl<'lines> DiffView<'lines> {
    /// Create a diff view.
    #[must_use]
    pub const fn new(lines: &'lines [DiffLine]) -> Self {
        Self {
            lines,
            mode: DiffViewMode::Unified,
            styles: DiffViewStyles {
                file_header: Style::new(),
                hunk_header: Style::new(),
                context: Style::new(),
                added: Style::new(),
                removed: Style::new(),
                gutter: Style::new(),
            },
            fold_context_threshold: None,
            fold_context_keep: 3,
        }
    }

    /// Set the rendering mode.
    #[must_use]
    pub const fn mode(mut self, mode: DiffViewMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set render styles.
    #[must_use]
    pub const fn styles(mut self, styles: DiffViewStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Fold long runs of unchanged context lines.
    ///
    /// Runs longer than `threshold` keep `keep_edges` lines at each edge and
    /// render the middle as one folded summary row.
    #[must_use]
    pub const fn fold_context(mut self, threshold: usize, keep_edges: usize) -> Self {
        self.fold_context_threshold = Some(threshold);
        self.fold_context_keep = keep_edges;
        self
    }

    /// Disable context folding.
    #[must_use]
    pub const fn without_context_folding(mut self) -> Self {
        self.fold_context_threshold = None;
        self
    }

    /// Return line count.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Return hunk header indices.
    #[must_use]
    pub fn hunk_indices(&self) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| (line.kind == DiffLineKind::HunkHeader).then_some(index))
            .collect()
    }

    /// Move state to the next hunk header, if one exists.
    pub fn next_hunk(&self, state: &mut DiffViewState) -> bool {
        state.next_hunk(self.lines)
    }

    /// Move state to the previous hunk header, if one exists.
    pub fn previous_hunk(&self, state: &mut DiffViewState) -> bool {
        state.previous_hunk(self.lines)
    }

    const fn resolved_mode(&self, area: Rect) -> DiffViewMode {
        match self.mode {
            DiffViewMode::Responsive if area.width >= 120 => DiffViewMode::SideBySide,
            DiffViewMode::Responsive => DiffViewMode::Unified,
            mode => mode,
        }
    }
}

impl StatefulWidget for DiffView<'_> {
    type State = DiffViewState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        let rows = self.render_rows();
        state.offset = state.offset.min(rows.len().saturating_sub(1));
        for (row, render_row) in rows
            .iter()
            .skip(state.offset)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            let rendered = match *render_row {
                DiffRenderRow::Line(index) => {
                    let line = &self.lines[index];
                    match self.resolved_mode(area) {
                        DiffViewMode::Unified | DiffViewMode::Responsive => {
                            self.render_unified_line(line)
                        }
                        DiffViewMode::SideBySide => self.render_side_by_side_line(line, area.width),
                    }
                }
                DiffRenderRow::Fold { count, .. } => self.render_fold_line(count),
            };
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &rendered,
            );
        }
    }
}

impl DiffView<'_> {
    fn render_rows(&self) -> Vec<DiffRenderRow> {
        let Some(threshold) = self.fold_context_threshold else {
            return (0..self.lines.len()).map(DiffRenderRow::Line).collect();
        };
        let mut rows = Vec::new();
        let mut index = 0usize;
        while index < self.lines.len() {
            if self.lines[index].kind != DiffLineKind::Context {
                rows.push(DiffRenderRow::Line(index));
                index = index.saturating_add(1);
                continue;
            }
            let start = index;
            while index < self.lines.len() && self.lines[index].kind == DiffLineKind::Context {
                index = index.saturating_add(1);
            }
            let count = index.saturating_sub(start);
            if count <= threshold || self.fold_context_keep.saturating_mul(2) >= count {
                rows.extend((start..index).map(DiffRenderRow::Line));
                continue;
            }
            rows.extend(
                (start..start.saturating_add(self.fold_context_keep)).map(DiffRenderRow::Line),
            );
            let fold_count = count.saturating_sub(self.fold_context_keep.saturating_mul(2));
            rows.push(DiffRenderRow::Fold {
                start: start.saturating_add(self.fold_context_keep),
                count: fold_count,
            });
            rows.extend(
                (index.saturating_sub(self.fold_context_keep)..index).map(DiffRenderRow::Line),
            );
        }
        rows
    }

    fn render_unified_line(&self, line: &DiffLine) -> Line {
        let style = self.style_for(line.kind);
        let prefix = match line.kind {
            DiffLineKind::Added => "+",
            DiffLineKind::Removed => "-",
            DiffLineKind::Context | DiffLineKind::FileHeader | DiffLineKind::HunkHeader => " ",
        };
        let mut spans = vec![
            Span::styled(format_unified_gutter(line), self.styles.gutter),
            Span::styled(prefix, style),
        ];
        spans.extend(self.content_spans(line, style));
        Line::from_spans(spans)
    }

    fn render_fold_line(&self, count: usize) -> Line {
        Line::from_spans(vec![Span::styled(
            format!("   ·    ·  ⋯ {count} unchanged lines folded"),
            self.styles.gutter,
        )])
    }

    fn render_side_by_side_line(&self, line: &DiffLine, width: u16) -> Line {
        let style = self.style_for(line.kind);
        let half = usize::from(width.saturating_sub(1) / 2);
        let old = match line.kind {
            DiffLineKind::Added => empty_side_spans(half, style),
            _ => side_spans(line.old_line, line, half, style),
        };
        let new = match line.kind {
            DiffLineKind::Removed => empty_side_spans(half, style),
            _ => side_spans(line.new_line, line, half, style),
        };
        let mut spans = old;
        spans.push(Span::styled("│", self.styles.gutter));
        spans.extend(new);
        Line::from_spans(spans)
    }

    fn content_spans(&self, line: &DiffLine, base_style: Style) -> Vec<Span> {
        if line.inline_spans.is_empty() {
            return vec![Span::styled(line.content.clone(), base_style)];
        }
        line.inline_spans
            .iter()
            .map(|span| Span::styled(span.content.clone(), base_style.patch(span.style)))
            .collect()
    }

    const fn style_for(&self, kind: DiffLineKind) -> Style {
        match kind {
            DiffLineKind::FileHeader => self.styles.file_header,
            DiffLineKind::HunkHeader => self.styles.hunk_header,
            DiffLineKind::Context => self.styles.context,
            DiffLineKind::Added => self.styles.added,
            DiffLineKind::Removed => self.styles.removed,
        }
    }
}

fn format_unified_gutter(line: &DiffLine) -> String {
    format!(
        "{:>4} {:>4} ",
        line.old_line
            .map_or_else(|| String::from("-"), |line| line.to_string()),
        line.new_line
            .map_or_else(|| String::from("-"), |line| line.to_string())
    )
}

fn empty_side_spans(width: usize, style: Style) -> Vec<Span> {
    vec![Span::styled(" ".repeat(width), style)]
}

fn side_spans(line_number: Option<u32>, line: &DiffLine, width: usize, style: Style) -> Vec<Span> {
    let gutter = format!(
        "{:>4} ",
        line_number.map_or_else(|| String::from("-"), |line| line.to_string())
    );
    let mut spans = vec![Span::styled(gutter.clone(), style)];
    let content_width =
        width.saturating_sub(unicode_width::UnicodeWidthStr::width(gutter.as_str()));
    if line.inline_spans.is_empty() {
        spans.push(Span::styled(
            truncate_to_width(&line.content, content_width),
            style,
        ));
        return pad_spans_to_width(spans, width, style);
    }
    let mut remaining = content_width;
    for inline in &line.inline_spans {
        if remaining == 0 {
            break;
        }
        let text = truncate_to_width(&inline.content, remaining);
        remaining = remaining.saturating_sub(unicode_width::UnicodeWidthStr::width(text.as_str()));
        spans.push(Span::styled(text, style.patch(inline.style)));
    }
    pad_spans_to_width(spans, width, style)
}

fn pad_spans_to_width(mut spans: Vec<Span>, width: usize, style: Style) -> Vec<Span> {
    let current = spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_str()))
        .sum::<usize>();
    if current < width {
        spans.push(Span::styled(
            " ".repeat(width.saturating_sub(current)),
            style,
        ));
    }
    spans
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0usize;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = unicode_width::UnicodeWidthStr::width(grapheme);
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        result.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        DiffFileList, DiffFileListState, DiffFileSummary, DiffInlineSpan, DiffLine, DiffLineKind,
        DiffView, DiffViewMode, DiffViewState,
    };
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::style::{Color, Modifier, Style};
    use crate::widget::StatefulWidget;

    #[test]
    fn diff_file_summary_displays_paths() {
        assert_eq!(
            DiffFileSummary::new("src/lib.rs", 3, 1).display_path(),
            "src/lib.rs"
        );
        assert_eq!(
            DiffFileSummary::renamed("old.rs", "new.rs", 1, 2).display_path(),
            "old.rs → new.rs"
        );
    }

    #[test]
    fn diff_file_list_renders_visible_files_and_selection() {
        let files = vec![
            DiffFileSummary::new("a.rs", 1, 0),
            DiffFileSummary::new("b.rs", 2, 3),
            DiffFileSummary::new("c.rs", 0, 4),
        ];
        let mut state = DiffFileListState {
            selected: Some(2),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 2));
        let mut frame = Frame::new(&mut buffer);

        DiffFileList::new(&files).render(Rect::new(0, 0, 16, 2), &mut frame, &mut state);

        assert_eq!(state.offset, 1);
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("b.rs +2 -3      ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("c.rs +0 -4      ")
        );
    }

    #[test]
    fn diff_view_renders_unified_lines() {
        let lines = vec![
            DiffLine::new(DiffLineKind::Removed, Some(1), None, "old"),
            DiffLine::new(DiffLineKind::Added, None, Some(1), "new"),
        ];
        let mut state = DiffViewState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 2));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines).render(Rect::new(0, 0, 16, 2), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("   1    - -old  ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("   -    1 +new  ")
        );
    }

    #[test]
    fn diff_view_renders_side_by_side_lines() {
        let lines = vec![
            DiffLine::new(DiffLineKind::Removed, Some(2), None, "old"),
            DiffLine::new(DiffLineKind::Added, None, Some(2), "new"),
        ];
        let mut state = DiffViewState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines).mode(DiffViewMode::SideBySide).render(
            Rect::new(0, 0, 20, 2),
            &mut frame,
            &mut state,
        );

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("   2 old │          ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("         │   2 new  ")
        );
    }

    #[test]
    fn diff_view_virtualizes_by_state_offset() {
        let lines = vec![
            DiffLine::new(DiffLineKind::Context, Some(1), Some(1), "one"),
            DiffLine::new(DiffLineKind::Context, Some(2), Some(2), "two"),
        ];
        let mut state = DiffViewState { offset: 1 };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 1));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines).render(Rect::new(0, 0, 16, 1), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("   2    2  two  ")
        );
    }

    #[test]
    fn diff_view_renders_inline_changed_spans() {
        let highlight = Style::new().bg(Color::Yellow).add_modifier(Modifier::BOLD);
        let lines = vec![
            DiffLine::new(DiffLineKind::Added, None, Some(1), "new value").inline_spans(vec![
                DiffInlineSpan::new("new ", Style::new()),
                DiffInlineSpan::new("value", highlight),
            ]),
        ];
        let mut state = DiffViewState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines)
            .styles(super::DiffViewStyles::default())
            .render(Rect::new(0, 0, 20, 1), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().get(Point::new(15, 0)).map(|cell| cell.style),
            Some(Style::new().fg(Color::Green).patch(highlight))
        );
    }

    #[test]
    fn diff_view_renders_side_by_side_inline_spans() {
        let highlight = Style::new().bg(Color::Yellow);
        let lines = vec![
            DiffLine::new(DiffLineKind::Removed, Some(1), None, "old value").inline_spans(vec![
                DiffInlineSpan::new("old ", Style::new()),
                DiffInlineSpan::new("value", highlight),
            ]),
        ];
        let mut state = DiffViewState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 1));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines)
            .styles(super::DiffViewStyles::default())
            .mode(DiffViewMode::SideBySide)
            .render(Rect::new(0, 0, 24, 1), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().get(Point::new(10, 0)).map(|cell| cell.style),
            Some(Style::new().fg(Color::Red).patch(highlight))
        );
    }

    #[test]
    fn diff_view_folds_long_context_ranges() {
        let lines = vec![
            DiffLine::new(DiffLineKind::HunkHeader, None, None, "@@"),
            DiffLine::new(DiffLineKind::Context, Some(1), Some(1), "one"),
            DiffLine::new(DiffLineKind::Context, Some(2), Some(2), "two"),
            DiffLine::new(DiffLineKind::Context, Some(3), Some(3), "three"),
            DiffLine::new(DiffLineKind::Context, Some(4), Some(4), "four"),
            DiffLine::new(DiffLineKind::Context, Some(5), Some(5), "five"),
            DiffLine::new(DiffLineKind::Added, None, Some(6), "six"),
        ];
        let mut state = DiffViewState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 5));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines).fold_context(3, 1).render(
            Rect::new(0, 0, 40, 5),
            &mut frame,
            &mut state,
        );

        assert_eq!(
            frame.buffer().row_symbols(2).as_deref(),
            Some("   ·    ·  ⋯ 3 unchanged lines folded   ")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("   5    5  five                         ")
        );
    }

    #[test]
    fn diff_view_navigates_hunks() {
        let lines = vec![
            DiffLine::new(DiffLineKind::FileHeader, None, None, "file"),
            DiffLine::new(DiffLineKind::HunkHeader, None, None, "@@ one @@"),
            DiffLine::new(DiffLineKind::Context, Some(1), Some(1), "one"),
            DiffLine::new(DiffLineKind::HunkHeader, None, None, "@@ two @@"),
            DiffLine::new(DiffLineKind::Added, None, Some(2), "two"),
        ];
        let view = DiffView::new(&lines);
        let mut state = DiffViewState::default();

        assert_eq!(view.hunk_indices(), vec![1, 3]);
        assert!(view.next_hunk(&mut state));
        assert_eq!(state.offset, 1);
        assert!(view.next_hunk(&mut state));
        assert_eq!(state.offset, 3);
        assert!(!view.next_hunk(&mut state));
        assert!(view.previous_hunk(&mut state));
        assert_eq!(state.offset, 1);
    }
}
