//! Diff/file view primitives.

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::style::{Color, Modifier, Style};
use crate::text::{Line, Span};
use crate::widget::StatefulWidget;

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
        }
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

/// Virtualized diff view widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffView<'lines> {
    lines: &'lines [DiffLine],
    mode: DiffViewMode,
    styles: DiffViewStyles,
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

    /// Return line count.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.lines.len()
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
        state.offset = state.offset.min(self.lines.len().saturating_sub(1));
        for (row, line) in self
            .lines
            .iter()
            .skip(state.offset)
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            let rendered = match self.resolved_mode(area) {
                DiffViewMode::Unified | DiffViewMode::Responsive => self.render_unified_line(line),
                DiffViewMode::SideBySide => self.render_side_by_side_line(line, area.width),
            };
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &rendered,
            );
        }
    }
}

impl DiffView<'_> {
    fn render_unified_line(&self, line: &DiffLine) -> Line {
        let style = self.style_for(line.kind);
        let prefix = match line.kind {
            DiffLineKind::Added => "+",
            DiffLineKind::Removed => "-",
            DiffLineKind::Context | DiffLineKind::FileHeader | DiffLineKind::HunkHeader => " ",
        };
        Line::from_spans(vec![
            Span::styled(format_unified_gutter(line), self.styles.gutter),
            Span::styled(prefix, style),
            Span::styled(line.content.clone(), style),
        ])
    }

    fn render_side_by_side_line(&self, line: &DiffLine, width: u16) -> Line {
        let style = self.style_for(line.kind);
        let half = usize::from(width.saturating_sub(1) / 2);
        let old = match line.kind {
            DiffLineKind::Added => String::new(),
            _ => side_text(line.old_line, &line.content, half),
        };
        let new = match line.kind {
            DiffLineKind::Removed => String::new(),
            _ => side_text(line.new_line, &line.content, half),
        };
        Line::from_spans(vec![
            Span::styled(pad_to_width(&old, half), style),
            Span::styled("│", self.styles.gutter),
            Span::styled(new, style),
        ])
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

fn side_text(line_number: Option<u32>, content: &str, width: usize) -> String {
    let text = format!(
        "{:>4} {}",
        line_number.map_or_else(|| String::from("-"), |line| line.to_string()),
        content
    );
    truncate_to_width(&text, width)
}

fn pad_to_width(text: &str, width: usize) -> String {
    let text = truncate_to_width(text, width);
    let padding = width.saturating_sub(unicode_width::UnicodeWidthStr::width(text.as_str()));
    format!("{text}{}", " ".repeat(padding))
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
    use super::{DiffLine, DiffLineKind, DiffView, DiffViewMode, DiffViewState};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::widget::StatefulWidget;

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
}
