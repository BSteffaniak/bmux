//! Pane text rules (redact, restyle) applied as pane rows are painted from the client's replica.
//!
//! A row is matched only when it is painted, which is only when the grid or something over it
//! changed: rows the frame skips cost nothing. A matched stretch is painted over in the same
//! frame, before the frame is diffed against the host, so masked text is never written to the
//! host terminal. With no rules, painting is untouched.

use std::sync::Arc;

use bmux_plugin::{AttachRenderExtension, PaneTextAction, PaneTextMatch, PaneTextRules};
use bmux_terminal_grid::PhysicalRow;

/// The pane text rules the frame's extensions answered with.
#[derive(Debug, Default)]
pub(super) struct PaneTextTransform {
    rules: Vec<Arc<PaneTextRules>>,
    /// Identifies this set of rules (0 for none): panes drawn with another are drawn again.
    pub(super) key: u64,
}

impl PaneTextTransform {
    pub(super) fn from_extensions(extensions: &[Arc<dyn AttachRenderExtension>]) -> Self {
        let rules = extensions
            .iter()
            .filter_map(|extension| extension.pane_text_rules())
            .filter(|rules| !rules.is_empty())
            .collect::<Vec<_>>();
        let key = rules.iter().fold(0_u64, |key, rules| {
            key.rotate_left(17) ^ rules.id().wrapping_mul(0x9E37_79B9_7F4A_7C15)
        });
        Self { rules, key }
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Columns `start..end` of a row, which `action` transforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowTextSpan {
    pub(super) start: u16,
    pub(super) end: u16,
    pub(super) action: PaneTextAction,
}

/// Buffers reused from row to row and frame to frame while matching (each pane keeps its own).
#[derive(Debug, Default)]
pub struct RowTextScratch {
    text: String,
    /// Each cell's first byte in `text`, its column and its width.
    cells: Vec<(usize, u16, u16)>,
    pub(super) spans: Vec<RowTextSpan>,
}

impl RowTextScratch {
    /// Match `transform`'s rules against columns `0..width` of `row`, leaving the stretches they
    /// transform in [`Self::spans`] (later rules over earlier ones).
    pub(super) fn match_row(
        &mut self,
        transform: &PaneTextTransform,
        row: &PhysicalRow,
        width: u16,
    ) {
        self.spans.clear();
        if transform.is_empty() {
            return;
        }
        self.text.clear();
        self.cells.clear();
        let mut col = 0_u16;
        while col < width {
            let cell = row.cells().get(usize::from(col));
            if cell.is_some_and(bmux_terminal_grid::Cell::is_wide_continuation) {
                col += 1;
                continue;
            }
            let (text, cell_width) = cell.map_or((" ", 1), |cell| {
                let text = match cell.text() {
                    "" => " ",
                    text => text,
                };
                (text, u16::from(cell.width()).clamp(1, 2))
            });
            self.cells.push((self.text.len(), col, cell_width));
            self.text.push_str(text);
            col = col.saturating_add(cell_width);
        }
        if self.text.bytes().all(|byte| byte == b' ') {
            return;
        }
        let mut found: Vec<PaneTextMatch<'_>> = Vec::new();
        for rules in &transform.rules {
            rules.matches(&self.text, &mut found);
        }
        for matched in found {
            let (start, end) = matched.bytes;
            let first = self.cells.partition_point(|(byte, _, _)| *byte <= start) - 1;
            let last = self.cells.partition_point(|(byte, _, _)| *byte < end) - 1;
            let (_, start_col, _) = self.cells[first];
            let (_, last_col, last_width) = self.cells[last];
            self.spans.push(RowTextSpan {
                start: start_col,
                end: last_col.saturating_add(last_width).min(width),
                action: *matched.action,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{PaneTextPattern, PaneTextRule};

    fn transform(pattern: &str) -> PaneTextTransform {
        let rules = PaneTextRules::new([PaneTextRule {
            id: "test".into(),
            pattern: PaneTextPattern::Regex(pattern.into()),
            action: PaneTextAction {
                mask: Some('*'),
                style: None,
            },
        }])
        .expect("rule compiles");
        PaneTextTransform {
            rules: vec![Arc::new(rules)],
            key: 1,
        }
    }

    fn row(text: &str) -> PhysicalRow {
        let mut grid = bmux_terminal_grid::TerminalGridStream::new(
            40,
            1,
            bmux_terminal_grid::GridLimits::default(),
        )
        .expect("grid");
        grid.process(text.as_bytes());
        grid.grid().viewport_row_ref(0).expect("row").clone()
    }

    /// A match is the columns its text occupies, a wide character counting two; no rules or a
    /// blank row match nothing.
    #[test]
    fn matches_map_to_the_columns_their_text_occupies() {
        let mut scratch = RowTextScratch::default();
        scratch.match_row(&transform("key=[A-Z]+"), &row("界 key=ABC done"), 40);
        let spans = scratch
            .spans
            .iter()
            .map(|span| (span.start, span.end))
            .collect::<Vec<_>>();
        assert_eq!(spans, vec![(3, 10)]);

        scratch.match_row(&PaneTextTransform::default(), &row("key=ABC"), 40);
        assert_eq!(scratch.spans, []);
        scratch.match_row(&transform(" +"), &row(""), 40);
        assert_eq!(scratch.spans, [], "a blank row is not matched");
    }
}
