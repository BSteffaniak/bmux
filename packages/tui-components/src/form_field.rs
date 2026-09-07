//! Reusable labeled form-field component.

use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId,
    LayoutMetadata, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier};

/// Styles for a [`FormField`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormFieldStyles {
    /// Label style.
    pub label: Style,
    /// Required marker style.
    pub required_marker: Style,
    /// Help text style.
    pub help: Style,
    /// Error text style.
    pub error: Style,
}

impl Default for FormFieldStyles {
    fn default() -> Self {
        Self {
            label: Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
            required_marker: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            help: Style::new().fg(Color::BrightBlack),
            error: Style::new().fg(Color::Red),
        }
    }
}

/// Canonical child-owning form-field component.
pub struct FormFieldComponent<'a> {
    id: LayoutId,
    field: FormField<'a>,
    control: Element<'a>,
}

impl<'a> FormFieldComponent<'a> {
    /// Create a form field around one measurable control.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, label: &'a str, control: impl Component + 'a) -> Self {
        Self {
            id: id.into(),
            field: FormField::new(label),
            control: Element::new(control),
        }
    }

    /// Mark this field as required.
    #[must_use]
    pub const fn required(mut self, required: bool) -> Self {
        self.field.required = required;
        self
    }

    /// Set optional help text.
    #[must_use]
    pub const fn help(mut self, help: &'a str) -> Self {
        self.field.help = Some(help);
        self
    }

    /// Set optional validation error text.
    #[must_use]
    pub const fn error(mut self, error: &'a str) -> Self {
        self.field.error = Some(error);
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: FormFieldStyles) -> Self {
        self.field.styles = styles;
        self
    }
}

impl Component for FormFieldComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.field.label.hash(&mut layout);
        self.field.required.hash(&mut layout);
        self.field.help.hash(&mut layout);
        self.field.error.hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.field.styles.label.hash(&mut paint);
        self.field.styles.required_marker.hash(&mut paint);
        self.field.styles.help.hash(&mut paint);
        self.field.styles.error.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish()).combine(self.control.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let footer_height = usize::from(self.field.help.is_some())
            .saturating_add(usize::from(self.field.error.is_some()));
        let chrome_height = 1usize.saturating_add(footer_height);
        let control = self.control.layout(
            Constraints::new(
                constraints.min_width(),
                constraints.max_width(),
                0,
                constraints
                    .max_height()
                    .map(|height| height.saturating_sub(chrome_height)),
            ),
            cx,
        );
        let label_width = bmux_tui::text_width::display_width(self.field.label)
            .saturating_add(usize::from(self.field.required) * 2);
        let footer_width = self
            .field
            .help
            .into_iter()
            .chain(self.field.error)
            .map(bmux_tui::text_width::display_width)
            .max()
            .unwrap_or_default();
        let width = control
            .size
            .width
            .max(u16::try_from(label_width.max(footer_width)).unwrap_or(u16::MAX));
        let size = constraints.constrain(LogicalSize::new(
            width,
            control.size.height.saturating_add(chrome_height),
        ));
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![ChildLayout::new(0, size.height.min(1), control)],
        )
        .with_metadata(LayoutMetadata::new().semantic("form-field"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        cx.write_line(
            LocalRect::new(0, 0, layout.size.width, 1),
            &self.field.label_line(),
        );
        let Some(child) = layout.children.first() else {
            return;
        };
        cx.with_child_size(
            i32::from(child.x),
            i64::try_from(child.y).unwrap_or(i64::MAX),
            child.node.size,
            |cx| self.control.paint(&child.node, cx),
        );
        let mut row = child.node.size.height.saturating_add(1);
        if let Some(help) = self.field.help
            && row < layout.size.height
        {
            cx.write_line(
                LocalRect::new(
                    0,
                    i64::try_from(row).unwrap_or(i64::MAX),
                    layout.size.width,
                    1,
                ),
                &Line::from_spans([Span::styled(help, self.field.styles.help)]),
            );
            row = row.saturating_add(1);
        }
        if let Some(error) = self.field.error
            && row < layout.size.height
        {
            cx.write_line(
                LocalRect::new(
                    0,
                    i64::try_from(row).unwrap_or(i64::MAX),
                    layout.size.width,
                    1,
                ),
                &Line::from_spans([Span::styled(error, self.field.styles.error)]),
            );
        }
        cx.with_child_size(0, 0, layout.size, |cx| {
            let area = cx.area();
            cx.push_semantic_in(
                area,
                SemanticRegion::new(self.id.as_str(), Rect::default(), "form-field"),
            );
            cx.push_damage(area);
        });
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(child) = layout.children.first() else {
            return EventOutcome::Ignored;
        };
        cx.with_child(child, |cx| self.control.event(event, &child.node, cx))
    }
}

/// Labeled form-field layout with optional required marker, help text, and error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField<'a> {
    label: &'a str,
    required: bool,
    help: Option<&'a str>,
    error: Option<&'a str>,
    styles: FormFieldStyles,
}

impl<'a> FormField<'a> {
    /// Create a labeled form field.
    #[must_use]
    pub const fn new(label: &'a str) -> Self {
        Self {
            label,
            required: false,
            help: None,
            error: None,
            styles: FormFieldStyles {
                label: Style::new(),
                required_marker: Style::new(),
                help: Style::new(),
                error: Style::new(),
            },
        }
    }

    /// Mark this field as required.
    #[must_use]
    pub const fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    /// Set optional help text.
    #[must_use]
    pub const fn help(mut self, help: &'a str) -> Self {
        self.help = Some(help);
        self
    }

    /// Set optional validation error text.
    #[must_use]
    pub const fn error(mut self, error: &'a str) -> Self {
        self.error = Some(error);
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: FormFieldStyles) -> Self {
        self.styles = styles;
        self
    }

    fn label_line(&self) -> Line {
        let mut spans = vec![Span::styled(self.label, self.styles.label)];
        if self.required {
            spans.push(Span::styled(" *", self.styles.required_marker));
        }
        Line::from_spans(spans)
    }
}

impl Default for FormField<'_> {
    fn default() -> Self {
        Self::new("").styles(FormFieldStyles::default())
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`FormFieldStyles`].
    #[must_use]
    pub fn form_field_styles(self) -> FormFieldStyles {
        FormFieldStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for FormFieldStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            label: theme.text.add_modifier(bmux_tui::style::Modifier::BOLD),
            required_marker: theme.error.add_modifier(bmux_tui::style::Modifier::BOLD),
            help: theme.muted,
            error: theme.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx, LogicalSize};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;
    use bmux_tui::prelude::TextBlock;

    use super::{FormFieldComponent, FormFieldStyles};

    #[test]
    fn component_measures_and_paints_child_with_chrome() {
        let component = FormFieldComponent::new("name", "Name", TextBlock::new("input"))
            .required(true)
            .help("Enter name")
            .error("Missing")
            .styles(FormFieldStyles::default());
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(20, 4));
        assert_eq!(layout.children[0].y, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Name *              ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("input               ")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("Missing             ")
        );
        assert_eq!(frame.semantics().regions().len(), 1);
    }

    #[test]
    fn tall_control_keeps_last_row_and_footer_visible() {
        use bmux_tui::prelude::{Line, Text};

        let control = TextBlock::new(Text::from_lines(vec![Line::from("input"); 70_001]));
        let component = FormFieldComponent::new("field", "Label", control).help("Help");
        let layout = component.layout(Constraints::new(5, 5, 0, None), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 70_003);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child_size(0, -70_001, layout.size, |cx| {
            component.paint(&layout, cx);
        });
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("input"));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("Help "));
        let region = &frame.semantics().regions()[0];
        assert_eq!(region.id, "field");
        assert_eq!(region.area, Rect::new(0, 0, 5, 2));
        assert!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_full()
        );
    }

    #[test]
    fn control_events_use_translated_child_clip() {
        use bmux_tui::component::{EventCx, LayoutNode};
        use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;

        struct Control;
        impl Component for Control {
            fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf(
                    "control".into(),
                    constraints.constrain(LogicalSize::new(8, 1)),
                )
            }
            fn paint(&self, _: &LayoutNode, _: &mut PaintCx<'_, '_>) {}
            fn event(&self, event: &Event, _: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
                let Event::Mouse(mouse) = event else {
                    return EventOutcome::Ignored;
                };
                if cx.clip().is_some_and(|clip| clip.contains(mouse.position)) {
                    EventOutcome::Handled
                } else {
                    EventOutcome::Ignored
                }
            }
        }

        let component = FormFieldComponent::new("field", "Label", Control);
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let mut cx = EventCx::new(&layout);
        for (y, handled) in [(3, false), (4, true), (5, false)] {
            let event = Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(6, y),
            ));
            let outcome = cx.with_transform(0, 0, 5, 3, Rect::new(5, 3, 8, 3), |cx| {
                component.event(&event, &layout, cx)
            });
            assert_eq!(outcome.is_handled(), handled);
        }
    }

    #[test]
    fn empty_and_label_only_fields_keep_control_inside_bounds() {
        let component = FormFieldComponent::new("field", "Label", TextBlock::new("input"))
            .help("Help")
            .error("Error");
        for height in 0..=1 {
            let layout = component.layout(
                Constraints::new(5, 5, 0, Some(height)),
                &mut LayoutCx::new(),
            );
            assert_eq!(layout.size.height, height);
            let child = &layout.children[0];
            assert_eq!(child.node.size.height, 0);
            assert!(child.y + child.node.size.height <= layout.size.height);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("     "));
            assert_eq!(
                frame.buffer().row_symbols(0).as_deref(),
                Some(if height == 0 { "     " } else { "Label" })
            );
        }
    }

    #[test]
    fn component_constraints_limit_child_before_adding_chrome() {
        let component =
            FormFieldComponent::new("name", "Name", TextBlock::new("one two three four five"))
                .help("Help");
        let layout = component.layout(Constraints::new(8, 8, 0, Some(4)), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 4);
        assert_eq!(layout.children[0].node.size.height, 2);
    }
}
