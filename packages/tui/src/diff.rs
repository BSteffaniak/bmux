//! Diff/file view primitives.

use bmux_keyboard::{KeyCode, KeyStroke};

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::hit::{HitId, HitMap, HitRegion, HitRole};
use crate::style::{Color, Modifier, Style};
use crate::text::{Line, Span};
use crate::text_width::{display_width, truncate_to_display_width};
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

    /// Select an item by index.
    pub const fn select(&mut self, index: Option<usize>) {
        self.selected = index;
    }

    /// Move selection down by one file.
    pub fn select_next(&mut self, file_count: usize) {
        if file_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        self.selected = Some(
            self.selected
                .map_or(0, |selected| selected.saturating_add(1).min(file_count - 1)),
        );
    }

    /// Move selection up by one file.
    pub fn select_previous(&mut self, file_count: usize) {
        if file_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        self.selected = Some(
            self.selected
                .map_or(0, |selected| selected.saturating_sub(1)),
        );
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

/// Result of handling a key stroke for a changed-file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFileListKeyOutcome {
    /// The key was not recognized as file-list input.
    Ignored,
    /// Selection or scroll position changed.
    Moved,
    /// The selected file was activated.
    Activated(usize),
    /// The file-list interaction was canceled.
    Canceled,
}

/// Key handling policy for changed-file lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffFileListKeyHandler;

impl DiffFileListKeyHandler {
    /// Apply a key stroke to changed-file list state.
    pub fn handle_key(
        self,
        state: &mut DiffFileListState,
        file_count: usize,
        viewport_height: u16,
        stroke: KeyStroke,
    ) -> DiffFileListKeyOutcome {
        if !stroke.modifiers.is_empty() {
            return DiffFileListKeyOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Up => {
                state.select_previous(file_count);
                state.ensure_selected_visible(viewport_height, file_count);
                DiffFileListKeyOutcome::Moved
            }
            KeyCode::Down => {
                state.select_next(file_count);
                state.ensure_selected_visible(viewport_height, file_count);
                DiffFileListKeyOutcome::Moved
            }
            KeyCode::Home => {
                state.select(if file_count == 0 { None } else { Some(0) });
                state.ensure_selected_visible(viewport_height, file_count);
                DiffFileListKeyOutcome::Moved
            }
            KeyCode::End => {
                state.select(file_count.checked_sub(1));
                state.ensure_selected_visible(viewport_height, file_count);
                DiffFileListKeyOutcome::Moved
            }
            KeyCode::PageUp => {
                move_file_selection_by_page(
                    state,
                    file_count,
                    viewport_height,
                    FilePageDirection::Up,
                );
                DiffFileListKeyOutcome::Moved
            }
            KeyCode::PageDown => {
                move_file_selection_by_page(
                    state,
                    file_count,
                    viewport_height,
                    FilePageDirection::Down,
                );
                DiffFileListKeyOutcome::Moved
            }
            KeyCode::Enter => state.selected.map_or(
                DiffFileListKeyOutcome::Ignored,
                DiffFileListKeyOutcome::Activated,
            ),
            KeyCode::Escape => DiffFileListKeyOutcome::Canceled,
            KeyCode::Char(_)
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Space
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Insert
            | KeyCode::F(_) => DiffFileListKeyOutcome::Ignored,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilePageDirection {
    Up,
    Down,
}

fn move_file_selection_by_page(
    state: &mut DiffFileListState,
    file_count: usize,
    viewport_height: u16,
    direction: FilePageDirection,
) {
    if file_count == 0 {
        state.selected = None;
        state.offset = 0;
        return;
    }
    let page = usize::from(viewport_height.max(1));
    let selected = state.selected.unwrap_or(0).min(file_count - 1);
    let next = match direction {
        FilePageDirection::Up => selected.saturating_sub(page),
        FilePageDirection::Down => selected.saturating_add(page).min(file_count - 1),
    };
    state.select(Some(next));
    state.ensure_selected_visible(viewport_height, file_count);
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
    /// Register visible file-row hit regions.
    pub fn register_hits(
        &self,
        area: Rect,
        state: &DiffFileListState,
        hits: &mut HitMap,
        id_prefix: &str,
    ) {
        if area.is_empty() {
            return;
        }
        for (row, index) in (state.offset..self.files.len())
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            hits.push(
                HitRegion::new(
                    HitId::new(format!("{id_prefix}:{index}")),
                    Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                )
                .role(HitRole::ListItem),
            );
        }
    }

    /// Resolve a hit id generated by [`Self::register_hits`] into a file index.
    #[must_use]
    pub fn hit_file_index(id: &HitId, id_prefix: &str) -> Option<usize> {
        id.as_str()
            .strip_prefix(id_prefix)?
            .strip_prefix(':')?
            .parse()
            .ok()
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

/// One emphasized range inside a diff line's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffChangedRange {
    /// Start byte offset in [`DiffLine::content`].
    pub start: usize,
    /// End byte offset in [`DiffLine::content`].
    pub end: usize,
}

impl DiffChangedRange {
    /// Create a changed range.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Return true when this range is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
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
    /// Optional inline content spans for syntax highlighting.
    pub inline_spans: Vec<DiffInlineSpan>,
    /// Changed byte ranges inside this line's content.
    pub changed_ranges: Vec<DiffChangedRange>,
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
            changed_ranges: Vec::new(),
        }
    }

    /// Set inline content spans.
    #[must_use]
    pub fn inline_spans(mut self, spans: Vec<DiffInlineSpan>) -> Self {
        self.inline_spans = spans;
        self
    }

    /// Set changed content ranges.
    #[must_use]
    pub fn changed_ranges(mut self, ranges: Vec<DiffChangedRange>) -> Self {
        self.changed_ranges = ranges;
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
    /// Added line fallback text style.
    pub added: Style,
    /// Removed line fallback text style.
    pub removed: Style,
    /// Added line full-row background style.
    pub added_row: Style,
    /// Removed line full-row background style.
    pub removed_row: Style,
    /// Added changed-range emphasis style.
    pub added_emphasis: Style,
    /// Removed changed-range emphasis style.
    pub removed_emphasis: Style,
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
            added_row: Style::new().bg(Color::Indexed(22)),
            removed_row: Style::new().bg(Color::Indexed(52)),
            added_emphasis: Style::new().bg(Color::Indexed(28)),
            removed_emphasis: Style::new().bg(Color::Indexed(88)),
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
                added_row: Style::new(),
                removed_row: Style::new(),
                added_emphasis: Style::new(),
                removed_emphasis: Style::new(),
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

    /// Render diff rows as styled lines for a fixed width.
    #[must_use]
    pub fn render_lines(&self, width: u16, max_rows: usize) -> Vec<Line> {
        self.render_rows()
            .into_iter()
            .take(max_rows)
            .map(|render_row| self.render_row(render_row, width))
            .collect()
    }

    /// Return the rendered row count after context folding.
    #[must_use]
    pub fn rendered_row_count(&self) -> usize {
        self.render_rows().len()
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
            let rendered = self.render_row(*render_row, area.width);
            let row_area = Rect::new(area.x, area.y.saturating_add(row), area.width, 1);
            if let Some(style) = self.render_row_background(*render_row) {
                frame.fill(row_area, " ", style);
            }
            frame.write_line(row_area, &rendered);
        }
    }
}

impl DiffView<'_> {
    fn render_row_background(&self, render_row: DiffRenderRow) -> Option<Style> {
        match render_row {
            DiffRenderRow::Line(index) => self.line_row_style(&self.lines[index]),
            DiffRenderRow::Fold { .. } => None,
        }
    }

    const fn line_row_style(&self, line: &DiffLine) -> Option<Style> {
        match line.kind {
            DiffLineKind::Added => Some(self.styles.added_row),
            DiffLineKind::Removed => Some(self.styles.removed_row),
            DiffLineKind::FileHeader | DiffLineKind::HunkHeader | DiffLineKind::Context => None,
        }
    }

    fn render_row(&self, render_row: DiffRenderRow, width: u16) -> Line {
        match render_row {
            DiffRenderRow::Line(index) => {
                let line = &self.lines[index];
                match self.resolved_mode(Rect::new(0, 0, width, 1)) {
                    DiffViewMode::Unified | DiffViewMode::Responsive => {
                        self.render_unified_line(line)
                    }
                    DiffViewMode::SideBySide => self.render_side_by_side_line(line, width),
                }
            }
            DiffRenderRow::Fold { count, .. } => self.render_fold_line(count),
        }
    }

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
        let style = self.content_base_style(line);
        let gutter_style = self.gutter_style(line);
        let prefix = match line.kind {
            DiffLineKind::Added => "+",
            DiffLineKind::Removed => "-",
            DiffLineKind::Context | DiffLineKind::FileHeader | DiffLineKind::HunkHeader => " ",
        };
        let mut spans = vec![
            Span::styled(format_unified_gutter(line), gutter_style),
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
        let style = self.content_base_style(line);
        let half = usize::from(width.saturating_sub(1) / 2);
        let old = match line.kind {
            DiffLineKind::Added => empty_side_spans(half, style),
            _ => side_spans(
                line.old_line,
                line,
                half,
                style,
                self.emphasis_style(line.kind),
            ),
        };
        let new = match line.kind {
            DiffLineKind::Removed => empty_side_spans(half, style),
            _ => side_spans(
                line.new_line,
                line,
                half,
                style,
                self.emphasis_style(line.kind),
            ),
        };
        let mut spans = old;
        spans.push(Span::styled("│", self.styles.gutter));
        spans.extend(new);
        Line::from_spans(spans)
    }

    fn content_spans(&self, line: &DiffLine, base_style: Style) -> Vec<Span> {
        layered_content_spans(
            &line.content,
            &line.inline_spans,
            &line.changed_ranges,
            base_style,
            self.emphasis_style(line.kind),
        )
    }

    fn content_base_style(&self, line: &DiffLine) -> Style {
        let style = self.style_for(line.kind);
        self.line_row_style(line)
            .map_or(style, |row| row.patch(style))
    }

    fn gutter_style(&self, line: &DiffLine) -> Style {
        self.line_row_style(line)
            .map_or(self.styles.gutter, |row| row.patch(self.styles.gutter))
    }

    const fn emphasis_style(&self, kind: DiffLineKind) -> Style {
        match kind {
            DiffLineKind::Added => self.styles.added_emphasis,
            DiffLineKind::Removed => self.styles.removed_emphasis,
            DiffLineKind::FileHeader | DiffLineKind::HunkHeader | DiffLineKind::Context => {
                Style::new()
            }
        }
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

fn side_spans(
    line_number: Option<u32>,
    line: &DiffLine,
    width: usize,
    style: Style,
    emphasis_style: Style,
) -> Vec<Span> {
    let gutter = format!(
        "{:>4} ",
        line_number.map_or_else(|| String::from("-"), |line| line.to_string())
    );
    let mut spans = vec![Span::styled(gutter.clone(), style)];
    let content_width = width.saturating_sub(display_width(&gutter));
    let mut remaining = content_width;
    for span in layered_content_spans(
        &line.content,
        &line.inline_spans,
        &line.changed_ranges,
        style,
        emphasis_style,
    ) {
        if remaining == 0 {
            break;
        }
        let text = truncate_to_display_width(&span.content, remaining);
        remaining = remaining.saturating_sub(display_width(&text));
        spans.push(Span::styled(text, span.style));
    }
    pad_spans_to_width(spans, width, style)
}

fn pad_spans_to_width(mut spans: Vec<Span>, width: usize, style: Style) -> Vec<Span> {
    let current = spans
        .iter()
        .map(|span| display_width(span.content.as_str()))
        .sum::<usize>();
    if current < width {
        spans.push(Span::styled(
            " ".repeat(width.saturating_sub(current)),
            style,
        ));
    }
    spans
}

fn layered_content_spans(
    content: &str,
    inline_spans: &[DiffInlineSpan],
    changed_ranges: &[DiffChangedRange],
    base_style: Style,
    emphasis_style: Style,
) -> Vec<Span> {
    let source_spans = if inline_spans.is_empty() {
        vec![DiffInlineSpan::new(content, Style::new())]
    } else {
        inline_spans.to_vec()
    };
    let ranges = normalized_changed_ranges(content, changed_ranges);
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for inline in source_spans {
        let mut local_start = 0usize;
        let span_start = offset;
        let span_end = span_start.saturating_add(inline.content.len());
        for range in ranges
            .iter()
            .copied()
            .filter(|range| range.start < span_end && range.end > span_start)
        {
            let overlap_start = range.start.max(span_start).saturating_sub(span_start);
            let overlap_end = range.end.min(span_end).saturating_sub(span_start);
            if local_start < overlap_start {
                push_content_slice(
                    &mut spans,
                    &inline.content[local_start..overlap_start],
                    base_style.patch(inline.style),
                );
            }
            push_content_slice(
                &mut spans,
                &inline.content[overlap_start..overlap_end],
                base_style.patch(inline.style).patch(emphasis_style),
            );
            local_start = overlap_end;
        }
        if local_start < inline.content.len() {
            push_content_slice(
                &mut spans,
                &inline.content[local_start..],
                base_style.patch(inline.style),
            );
        }
        offset = span_end;
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }
    spans
}

fn push_content_slice(spans: &mut Vec<Span>, content: &str, style: Style) {
    if !content.is_empty() {
        spans.push(Span::styled(content.to_owned(), style));
    }
}

fn normalized_changed_ranges(
    content: &str,
    changed_ranges: &[DiffChangedRange],
) -> Vec<DiffChangedRange> {
    let mut ranges = changed_ranges
        .iter()
        .copied()
        .filter_map(|range| normalize_changed_range(content, range))
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<DiffChangedRange> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
            continue;
        }
        merged.push(range);
    }
    merged
}

fn normalize_changed_range(content: &str, range: DiffChangedRange) -> Option<DiffChangedRange> {
    let start = range.start.min(content.len());
    let end = range.end.min(content.len());
    if start >= end || !content.is_char_boundary(start) || !content.is_char_boundary(end) {
        return None;
    }
    Some(DiffChangedRange::new(start, end))
}

#[cfg(test)]
mod tests {
    use super::{
        DiffChangedRange, DiffFileList, DiffFileListKeyHandler, DiffFileListKeyOutcome,
        DiffFileListState, DiffFileSummary, DiffInlineSpan, DiffLine, DiffLineKind, DiffView,
        DiffViewMode, DiffViewState,
    };
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::{Point, Rect};
    use crate::hit::HitMap;
    use crate::style::{Color, Modifier, Style};
    use crate::widget::StatefulWidget;
    use bmux_keyboard::{KeyCode, KeyStroke};

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
    fn diff_file_list_key_handler_moves_and_activates() {
        let mut state = DiffFileListState::new();
        let handler = DiffFileListKeyHandler;

        assert_eq!(
            handler.handle_key(&mut state, 5, 2, KeyStroke::simple(KeyCode::Down)),
            DiffFileListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(0));
        assert_eq!(
            handler.handle_key(&mut state, 5, 2, KeyStroke::simple(KeyCode::PageDown)),
            DiffFileListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(2));
        assert_eq!(state.offset, 1);
        assert_eq!(
            handler.handle_key(&mut state, 5, 2, KeyStroke::simple(KeyCode::Enter)),
            DiffFileListKeyOutcome::Activated(2)
        );
    }

    #[test]
    fn diff_file_list_registers_visible_row_hits() {
        let files = vec![
            DiffFileSummary::new("a.rs", 1, 0),
            DiffFileSummary::new("b.rs", 2, 3),
            DiffFileSummary::new("c.rs", 0, 4),
        ];
        let list = DiffFileList::new(&files);
        let state = DiffFileListState {
            selected: Some(2),
            offset: 1,
        };
        let mut hits = HitMap::new();

        list.register_hits(Rect::new(2, 3, 20, 2), &state, &mut hits, "files");

        let hit = hits
            .hit_test(Point::new(3, 4))
            .expect("second visible file row should be hittable");
        assert_eq!(hit.id().as_str(), "files:2");
        assert_eq!(DiffFileList::hit_file_index(hit.id(), "files"), Some(2));
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
    fn diff_view_fills_changed_row_backgrounds() {
        let lines = vec![DiffLine::new(DiffLineKind::Added, None, Some(1), "new")];
        let mut state = DiffViewState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines)
            .styles(super::DiffViewStyles::default())
            .render(Rect::new(0, 0, 20, 1), &mut frame, &mut state);

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 0))
                .map(|cell| cell.style.bg),
            Some(Some(Color::Indexed(22)))
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(19, 0))
                .map(|cell| cell.style.bg),
            Some(Some(Color::Indexed(22)))
        );
    }

    #[test]
    fn diff_view_layers_changed_ranges_over_syntax_spans() {
        let syntax = Style::new().fg(Color::Cyan);
        let lines = vec![
            DiffLine::new(DiffLineKind::Added, None, Some(1), "new value")
                .inline_spans(vec![
                    DiffInlineSpan::new("new ", syntax),
                    DiffInlineSpan::new("value", syntax),
                ])
                .changed_ranges(vec![DiffChangedRange::new(4, 9)]),
        ];
        let mut state = DiffViewState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 1));
        let mut frame = Frame::new(&mut buffer);

        DiffView::new(&lines)
            .styles(super::DiffViewStyles::default())
            .render(Rect::new(0, 0, 24, 1), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().get(Point::new(11, 0)).map(|cell| cell.style),
            Some(Style::new().bg(Color::Indexed(22)).patch(syntax))
        );
        assert_eq!(
            frame.buffer().get(Point::new(15, 0)).map(|cell| cell.style),
            Some(Style::new().bg(Color::Indexed(28)).patch(syntax))
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
            Some(
                Style::new()
                    .bg(Color::Indexed(22))
                    .fg(Color::Green)
                    .patch(highlight)
            )
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
            Some(
                Style::new()
                    .bg(Color::Indexed(52))
                    .fg(Color::Red)
                    .patch(highlight)
            )
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
