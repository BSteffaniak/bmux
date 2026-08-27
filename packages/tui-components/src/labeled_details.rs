//! Reusable labeled detail list component.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata, LayoutNode,
    LogicalSize,
};
use bmux_tui::frame::Frame;
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

/// Canonical component-lifecycle labeled detail list.
pub struct LabeledDetailsComponent<'a> {
    id: LayoutId,
    details: LabeledDetails<'a>,
}

impl<'a> LabeledDetailsComponent<'a> {
    /// Create a labeled-details component with stable identity.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, items: &'a [DetailItem]) -> Self {
        Self {
            id: id.into(),
            details: LabeledDetails::new(items),
        }
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: LabeledDetailsStyles) -> Self {
        self.details.styles = styles;
        self
    }

    /// Set whether to insert a blank row between detail items.
    #[must_use]
    pub const fn item_spacing(mut self, item_spacing: bool) -> Self {
        self.details.item_spacing = item_spacing;
        self
    }
}

impl Component for LabeledDetailsComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for item in self.details.items {
            item.label.hash(&mut layout);
            item.value.hash(&mut layout);
        }
        self.details.item_spacing.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.details.styles.label.hash(&mut paint);
        self.details.styles.value.hash(&mut paint);
        self.details.styles.continuation.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            let intrinsic = self
                .details
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
                .unwrap_or_default();
            u16::try_from(intrinsic)
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        let height = self.details.lines(width).len();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
        )
        .with_metadata(LayoutMetadata::new().semantic("details"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        for (row, line) in self
            .details
            .lines(layout.size.width)
            .iter()
            .take(layout.size.height)
            .enumerate()
        {
            cx.write_line(
                LocalRect::new(
                    0,
                    i64::try_from(row).unwrap_or(i64::MAX),
                    layout.size.width,
                    1,
                ),
                line,
            );
        }
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = LocalRect::new(0, 0, layout.size.width, height);
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, height),
            "details",
        ));
        cx.push_damage(area);
    }
}

/// Vertical list of labeled, wrapped details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledDetails<'a> {
    items: &'a [DetailItem],
    styles: LabeledDetailsStyles,
    item_spacing: bool,
}

impl<'a> LabeledDetails<'a> {
    /// Create a labeled details component.
    #[must_use]
    pub fn new(items: &'a [DetailItem]) -> Self {
        Self {
            items,
            styles: LabeledDetailsStyles::default(),
            item_spacing: true,
        }
    }

    /// Set styles.
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

    /// Render this detail list to lines for a given width.
    #[must_use]
    pub fn lines(&self, width: u16) -> Vec<Line> {
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

    /// Render this detail list directly.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        self.render_lines(area, frame, None);
    }

    /// Render this detail list directly with a fallback style for each row.
    pub fn render_with_fallback_style(&self, area: Rect, frame: &mut Frame<'_>, style: Style) {
        self.render_lines(area, frame, Some(style));
    }

    fn render_lines(&self, area: Rect, frame: &mut Frame<'_>, fallback: Option<Style>) {
        if area.is_empty() {
            return;
        }
        for (index, line) in self
            .lines(area.width)
            .iter()
            .take(usize::from(area.height))
            .enumerate()
        {
            let Ok(offset) = u16::try_from(index) else {
                return;
            };
            let line_area = Rect::new(area.x, area.y.saturating_add(offset), area.width, 1);
            if let Some(fallback) = fallback {
                frame.write_line_with_fallback_style(line_area, line, fallback);
            } else {
                frame.write_line(line_area, line);
            }
        }
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

    use super::{DetailItem, LabeledDetails, LabeledDetailsComponent, LabeledDetailsStyles};

    #[test]
    fn wraps_detail_values() {
        let items = [DetailItem::new("command", "abcdef")];
        let lines = LabeledDetails::new(&items).lines(5);

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
