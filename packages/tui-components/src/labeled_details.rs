//! Reusable labeled detail list component.
//!
//! Labels sit on their own row above each value by default. With
//! [`LabelPlacement::Beside`] they form a left column with values beside them,
//! which fits roughly twice as many fields into a narrow side pane.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::{display_width, wrap_text_with_continuation};

/// One labeled detail item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailItem {
    /// Field label.
    pub label: String,
    /// Field value.
    pub value: String,
}

impl DetailItem {
    /// Create a detail item.
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Styles for a labeled detail list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabeledDetailsStyles {
    /// Label style.
    pub label: Style,
    /// Value style.
    pub value: Style,
    /// Continuation indentation style.
    pub continuation: Style,
}

impl Default for LabeledDetailsStyles {
    fn default() -> Self {
        Self {
            label: Style::new()
                .fg(Color::BrightBlack)
                .add_modifier(Modifier::BOLD),
            value: Style::new().fg(Color::BrightWhite),
            continuation: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Where each label is placed relative to its value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LabelPlacement {
    /// The label on its own row, with the value indented on the rows below.
    #[default]
    Above,
    /// Labels in a left column padded to the widest label, values beside
    /// them, and wrapped value rows aligned under the value column.
    ///
    /// Falls back to [`Self::Above`] when the width cannot fit the label
    /// column plus [`MIN_BESIDE_VALUE_WIDTH`] value columns.
    Beside,
}

/// Narrowest value column [`LabelPlacement::Beside`] uses before falling back
/// to [`LabelPlacement::Above`].
pub const MIN_BESIDE_VALUE_WIDTH: usize = 8;

/// Canonical component-lifecycle labeled detail list.
pub struct LabeledDetailsComponent<'a> {
    id: LayoutId,
    items: &'a [DetailItem],
    styles: LabeledDetailsStyles,
    item_spacing: bool,
    label_placement: LabelPlacement,
    label_gap: u16,
}

impl<'a> LabeledDetailsComponent<'a> {
    /// Create a labeled-details component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, items: &'a [DetailItem]) -> Self {
        Self {
            id: id.into(),
            items,
            styles: LabeledDetailsStyles::default(),
            item_spacing: true,
            label_placement: LabelPlacement::Above,
            label_gap: 2,
        }
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: LabeledDetailsStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set whether to insert a blank row between detail items.
    #[must_use]
    pub const fn item_spacing(mut self, item_spacing: bool) -> Self {
        self.item_spacing = item_spacing;
        self
    }

    /// Set where labels are placed relative to their values.
    #[must_use]
    pub const fn label_placement(mut self, label_placement: LabelPlacement) -> Self {
        self.label_placement = label_placement;
        self
    }

    /// Set the columns between the label column and values in
    /// [`LabelPlacement::Beside`] (default 2).
    #[must_use]
    pub const fn label_gap(mut self, label_gap: u16) -> Self {
        self.label_gap = label_gap;
        self
    }
}

impl Component for LabeledDetailsComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for item in self.items {
            item.label.hash(&mut layout);
            item.value.hash(&mut layout);
        }
        self.item_spacing.hash(&mut layout);
        self.label_placement.hash(&mut layout);
        self.label_gap.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.styles.label.hash(&mut paint);
        self.styles.value.hash(&mut paint);
        self.styles.continuation.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            let intrinsic = match self.label_placement {
                LabelPlacement::Above => self
                    .items
                    .iter()
                    .flat_map(|item| {
                        std::iter::once(display_width(&item.label)).chain(
                            item.value
                                .lines()
                                .map(|line| display_width(line).saturating_add(2)),
                        )
                    })
                    .max()
                    .unwrap_or_default(),
                LabelPlacement::Beside => self.label_column_width().saturating_add(
                    self.items
                        .iter()
                        .flat_map(|item| item.value.lines().map(display_width))
                        .max()
                        .unwrap_or_default()
                        .max(MIN_BESIDE_VALUE_WIDTH),
                ),
            };
            u16::try_from(intrinsic)
                .unwrap_or(u16::MAX)
                .clamp(
                    constraints.min_width().try_into().unwrap_or(u16::MAX),
                    constraints.max_width().try_into().unwrap_or(u16::MAX),
                )
                .into()
        };
        let height = self.lines(u16::try_from(width).unwrap_or(u16::MAX)).len();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                width,
                height.try_into().unwrap_or(u64::MAX),
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("details"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        for (row, line) in self
            .lines(layout.size.width.try_into().unwrap_or(u16::MAX))
            .iter()
            .take(layout.size.height.try_into().unwrap_or(usize::MAX))
            .enumerate()
        {
            cx.write_line(
                LocalRect::new(
                    0,
                    i64::try_from(row).unwrap_or(i64::MAX),
                    layout.size.width.try_into().unwrap_or(u16::MAX),
                    1,
                ),
                line,
            );
        }
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = LocalRect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            height,
        );
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(
                0,
                0,
                layout.size.width.try_into().unwrap_or(u16::MAX),
                height,
            ),
            "details",
        ));
        cx.push_damage(area);
    }
}

impl LabeledDetailsComponent<'_> {
    /// Build this detail list's rows for a given width.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line> {
        let label_width = self.label_column_width();
        if self.label_placement == LabelPlacement::Beside
            && usize::from(width) >= label_width.saturating_add(MIN_BESIDE_VALUE_WIDTH)
        {
            return self.beside_lines(width, label_width);
        }
        let mut rows = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            rows.push(Line::from_spans(vec![Span::styled(
                item.label.clone(),
                self.styles.label,
            )]));
            for line in item.value.lines() {
                push_wrapped_value(&mut rows, line, width, self.styles);
            }
            if self.item_spacing && index + 1 < self.items.len() {
                rows.push(Line::default());
            }
        }
        rows
    }
}

impl LabeledDetailsComponent<'_> {
    /// Widest label plus the gap.
    fn label_column_width(&self) -> usize {
        self.items
            .iter()
            .map(|item| display_width(&item.label))
            .max()
            .unwrap_or_default()
            .saturating_add(usize::from(self.label_gap))
    }

    fn beside_lines(&self, width: u16, label_width: usize) -> Vec<Line> {
        let value_width = usize::from(width).saturating_sub(label_width);
        let indent = " ".repeat(label_width);
        let mut rows = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let padding = " ".repeat(label_width.saturating_sub(display_width(&item.label)));
            let mut first = true;
            let value_lines = item
                .value
                .lines()
                .chain(item.value.is_empty().then_some(""));
            for line in value_lines {
                for chunk in wrap_text_with_continuation(line, value_width, value_width) {
                    let mut spans = if first {
                        vec![
                            Span::styled(item.label.clone(), self.styles.label),
                            Span::styled(padding.clone(), self.styles.continuation),
                        ]
                    } else {
                        vec![Span::styled(indent.clone(), self.styles.continuation)]
                    };
                    first = false;
                    spans.push(Span::styled(chunk, self.styles.value));
                    rows.push(Line::from_spans(spans));
                }
            }
            if self.item_spacing && index + 1 < self.items.len() {
                rows.push(Line::default());
            }
        }
        rows
    }
}

fn push_wrapped_value(rows: &mut Vec<Line>, text: &str, width: u16, styles: LabeledDetailsStyles) {
    let max_width = usize::from(width.max(1));
    let prefix = "  ";
    let first_width = max_width.saturating_sub(display_width(prefix)).max(1);
    let next_width = first_width;
    for chunk in wrap_text_with_continuation(text, first_width, next_width) {
        rows.push(Line::from_spans(vec![
            Span::styled(prefix, styles.continuation),
            Span::styled(chunk, styles.value),
        ]));
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`LabeledDetailsStyles`].
    #[must_use]
    pub fn labeled_details_styles(self) -> LabeledDetailsStyles {
        LabeledDetailsStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for LabeledDetailsStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            label: theme.muted.add_modifier(bmux_tui::style::Modifier::BOLD),
            value: theme.text,
            continuation: theme.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;
    use bmux_tui::style::{Color, Style};

    use super::{DetailItem, LabelPlacement, LabeledDetailsComponent, LabeledDetailsStyles};

    fn plain(lines: &[bmux_tui::prelude::Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.plain_text().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn beside_aligns_labels_and_wraps_under_the_value_column() {
        let items = [
            DetailItem::new("Key", "openai/gpt-6"),
            DetailItem::new("Capabilities", "tools reasoning vision"),
            DetailItem::new("Notes", "line one\nline two"),
        ];
        let lines = LabeledDetailsComponent::new("details", &items)
            .item_spacing(false)
            .label_placement(LabelPlacement::Beside)
            .lines(26);
        assert_eq!(
            plain(&lines),
            [
                "Key           openai/gpt-6",
                "Capabilities  tools",
                "              reasoning",
                "              vision",
                "Notes         line one",
                "              line two",
            ]
        );
    }

    #[test]
    fn beside_keeps_rows_for_empty_values_and_honors_gap_and_spacing() {
        let items = [DetailItem::new("a", ""), DetailItem::new("bb", "x")];
        let lines = LabeledDetailsComponent::new("details", &items)
            .label_placement(LabelPlacement::Beside)
            .label_gap(1)
            .lines(20);
        assert_eq!(plain(&lines), ["a", "", "bb x"]);
    }

    #[test]
    fn beside_falls_back_to_above_when_too_narrow() {
        let items = [DetailItem::new("command", "abcdef")];
        let beside = LabeledDetailsComponent::new("details", &items)
            .label_placement(LabelPlacement::Beside)
            .lines(12);
        let above = LabeledDetailsComponent::new("details", &items).lines(12);
        assert_eq!(beside, above);
    }

    #[test]
    fn beside_measures_intrinsic_width_and_changes_layout_revision() {
        use bmux_tui::component::{Component, Constraints, LayoutCx, LogicalSize};

        let items = [DetailItem::new("id", "abc"), DetailItem::new("name", "x")];
        let component = LabeledDetailsComponent::new("details", &items)
            .item_spacing(false)
            .label_placement(LabelPlacement::Beside);
        let layout = component.layout(Constraints::new(0, 40, 0, None), &mut LayoutCx::new());
        // label column (4 + gap 2) + minimum value column.
        assert_eq!(layout.size, LogicalSize::new(14, 2));
        let above = LabeledDetailsComponent::new("details", &items).item_spacing(false);
        assert_ne!(component.revision().layout, above.revision().layout);
    }

    #[test]
    fn wraps_detail_values() {
        let items = [DetailItem::new("command", "abcdef")];
        let lines = LabeledDetailsComponent::new("details", &items).lines(5);

        assert_eq!(lines[0].plain_text(), "command");
        assert_eq!(lines[1].plain_text(), "  abc");
        assert_eq!(lines[2].plain_text(), "  def");
    }

    #[test]
    fn component_measures_wrapped_rows_and_paints_metadata() {
        let items = [
            DetailItem::new("command", "abcdef"),
            DetailItem::new("state", "ready"),
        ];
        let component = LabeledDetailsComponent::new("details", &items);
        let layout = component.layout(Constraints::for_width(5), &mut LayoutCx::new());
        assert_eq!(layout.size, bmux_tui::component::LogicalSize::new(5, 7));

        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 6));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("comma"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("  abc"));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert!(
            !frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_none()
        );
    }

    #[test]
    fn component_spacing_changes_layout_and_styles_only_change_paint() {
        let items = [DetailItem::new("one", "1"), DetailItem::new("two", "2")];
        let initial = LabeledDetailsComponent::new("details", &items).revision();
        let compact = LabeledDetailsComponent::new("details", &items)
            .item_spacing(false)
            .revision();
        let styled = LabeledDetailsComponent::new("details", &items)
            .styles(LabeledDetailsStyles {
                label: Style::new().fg(Color::Red),
                ..LabeledDetailsStyles::default()
            })
            .revision();
        assert_ne!(initial.layout, compact.layout);
        assert_eq!(initial.layout, styled.layout);
        assert_ne!(initial.paint, styled.paint);
    }

    #[test]
    fn component_clips_to_constrained_height() {
        let items = [DetailItem::new("command", "abcdef")];
        let component = LabeledDetailsComponent::new("details", &items);
        let layout = component.layout(Constraints::new(5, 5, 0, Some(2)), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("  abc"));
    }
}
