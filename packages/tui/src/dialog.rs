//! Button and dialog widgets.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use crate::chrome::{Border, Panel, PanelComponent};
use crate::component::{
    ChildLayout, Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use crate::geometry::Rect;
use crate::layout::{Direction, split_trailing};
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;
use crate::text::{Line, Text};
use crate::text_block::{TextBlock, TextWrap};

/// A simple button component.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    id: LayoutId,
    label: Line,
    style: Style,
    focused_style: Style,
    focused: bool,
}

impl Button {
    /// Create a button with a label.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, label: impl Into<Line>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            style: Style::new(),
            focused_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            focused: false,
        }
    }

    /// Set base style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set focused style.
    #[must_use]
    pub const fn focused_style(mut self, style: Style) -> Self {
        self.focused_style = style;
        self
    }

    /// Set focused state.
    #[must_use]
    pub const fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
    fn line(&self) -> Line {
        let style = if self.focused {
            self.style.patch(self.focused_style)
        } else {
            self.style
        };
        Line::from_spans(vec![
            crate::text::Span::styled("[ ", style),
            self.label
                .with_fallback_style(style)
                .spans
                .into_iter()
                .next()
                .unwrap_or_else(|| crate::text::Span::styled(String::new(), style)),
            crate::text::Span::styled(" ]", style),
        ])
    }
}

impl Component for Button {
    fn revision(&self) -> ComponentRevision {
        let line = self.line();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        line.width().hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{line:?}").hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = u16::try_from(self.line().width()).unwrap_or(u16::MAX);
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("button"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        cx.write_line(area, &self.line());
        cx.push_damage(area);
    }
}

/// A dialog action button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogAction {
    /// Stable action id chosen by the caller.
    pub id: String,
    /// Action label.
    pub label: Line,
}

impl DialogAction {
    /// Create a dialog action.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<Line>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// Selection state for dialog actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DialogState {
    /// Focused action index.
    pub focused_action: usize,
}

impl DialogState {
    /// Move focus to the next action.
    pub const fn focus_next(&mut self, action_count: usize) {
        if action_count == 0 {
            self.focused_action = 0;
        } else {
            self.focused_action = self.focused_action.saturating_add(1) % action_count;
        }
    }

    /// Move focus to the previous action.
    pub const fn focus_previous(&mut self, action_count: usize) {
        if action_count == 0 {
            self.focused_action = 0;
        } else if self.focused_action == 0 {
            self.focused_action = action_count.saturating_sub(1);
        } else {
            self.focused_action = self.focused_action.saturating_sub(1);
        }
    }
}

/// A generic modal-style dialog with body text and action buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog<'a> {
    panel: Panel,
    body: TextBlock,
    actions: &'a [DialogAction],
    button_style: Style,
    focused_button_style: Style,
}

impl<'a> Dialog<'a> {
    /// Create a dialog from body text and actions.
    #[must_use]
    pub fn new(body: impl Into<Text>, actions: &'a [DialogAction]) -> Self {
        Self {
            panel: Panel::new().border(Border::single()),
            body: TextBlock::new(body.into()).wrap(TextWrap::Word),
            actions,
            button_style: Style::new(),
            focused_button_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
        }
    }

    /// Set panel chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }

    /// Set button style.
    #[must_use]
    pub const fn button_style(mut self, style: Style) -> Self {
        self.button_style = style;
        self
    }

    /// Set focused button style.
    #[must_use]
    pub const fn focused_button_style(mut self, style: Style) -> Self {
        self.focused_button_style = style;
        self
    }

    /// Return the panel inner area.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        self.panel.inner_area(area)
    }
}

/// Canonical component-lifecycle dialog with caller-owned action focus.
pub struct DialogComponent<'a, 'state> {
    id: LayoutId,
    dialog: Dialog<'a>,
    state: &'state Cell<DialogState>,
}

impl<'a, 'state> DialogComponent<'a, 'state> {
    /// Wrap a dialog under stable identity with caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        dialog: Dialog<'a>,
        state: &'state Cell<DialogState>,
    ) -> Self {
        Self {
            id: id.into(),
            dialog,
            state,
        }
    }
}

impl Component for DialogComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.hash(&mut layout);
        format!("{:?}", self.dialog.body).hash(&mut layout);
        format!("{:?}", self.dialog.actions).hash(&mut layout);
        format!("{:?}", self.dialog.panel).hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.dialog.button_style.hash(&mut paint);
        self.dialog.focused_button_style.hash(&mut paint);
        self.state.get().focused_action.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let size = constraints.constrain(LogicalSize::new(
            constraints.max_width(),
            constraints
                .max_height()
                .unwrap_or_else(|| constraints.min_height()),
        ));
        let height = u16::try_from(size.height).unwrap_or(u16::MAX);
        let area = Rect::new(0, 0, size.width, height);
        let inner = self.dialog.content_area(area);
        let action_height = u16::from(!self.dialog.actions.is_empty());
        let split = split_trailing(inner, Direction::Vertical, action_height);
        let panel = PanelComponent::new(format!("{}.panel", self.id.as_str()), &self.dialog.panel);
        let mut panel_layout = panel.layout(Constraints::tight(area.size()), cx);
        panel_layout.children.push(ChildLayout::new(
            split.first.x,
            usize::from(split.first.y),
            self.dialog
                .body
                .layout(Constraints::tight(split.first.size()), cx),
        ));
        let mut x = split.second.x;
        let focused = self
            .state
            .get()
            .focused_action
            .min(self.dialog.actions.len().saturating_sub(1));
        self.state.set(DialogState {
            focused_action: focused,
        });
        for (index, action) in self.dialog.actions.iter().enumerate() {
            if x >= split.second.right() {
                break;
            }
            let width = u16::try_from(unicode_width::UnicodeWidthStr::width(
                action.label.plain_text().as_str(),
            ))
            .unwrap_or(u16::MAX)
            .saturating_add(4)
            .min(split.second.right().saturating_sub(x));
            let button = Button::new(format!("dialog-action:{}", action.id), action.label.clone())
                .style(self.dialog.button_style)
                .focused_style(self.dialog.focused_button_style)
                .focused(index == focused);
            panel_layout.children.push(ChildLayout::new(
                x,
                usize::from(split.second.y),
                button.layout(Constraints::tight(Rect::new(0, 0, width, 1).size()), cx),
            ));
            x = x.saturating_add(width).saturating_add(1);
        }
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![ChildLayout::new(0, 0, panel_layout)],
        )
        .with_metadata(LayoutMetadata::new().semantic("dialog"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(panel_layout) = layout.children.first() else {
            return;
        };
        let panel = PanelComponent::new(format!("{}.panel", self.id.as_str()), &self.dialog.panel);
        panel.paint(&panel_layout.node, cx);
        if let Some(body_layout) = panel_layout.node.children.first() {
            cx.with_child(
                i32::from(body_layout.x),
                i64::try_from(body_layout.y).unwrap_or(i64::MAX),
                LocalRect::new(
                    0,
                    0,
                    body_layout.node.size.width,
                    u16::try_from(body_layout.node.size.height).unwrap_or(u16::MAX),
                ),
                |cx| self.dialog.body.paint(&body_layout.node, cx),
            );
        }
        let focused = self
            .state
            .get()
            .focused_action
            .min(self.dialog.actions.len().saturating_sub(1));
        for (index, (action, child)) in self
            .dialog
            .actions
            .iter()
            .zip(panel_layout.node.children.iter().skip(1))
            .enumerate()
        {
            let button = Button::new(format!("dialog-action:{}", action.id), action.label.clone())
                .style(self.dialog.button_style)
                .focused_style(self.dialog.focused_button_style)
                .focused(index == focused);
            cx.with_child(
                i32::from(child.x),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                LocalRect::new(0, 0, child.node.size.width, 1),
                |cx| button.paint(&child.node, cx),
            );
        }
        cx.push_damage(LocalRect::new(
            0,
            0,
            layout.size.width,
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{Button, Dialog, DialogAction, DialogComponent, DialogState};
    use crate::buffer::Buffer;
    use crate::chrome::{Border, Panel};
    use crate::component::{Component, Constraints, LayoutCx};
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::paint::PaintCx;
    use crate::style::{Color, Style};
    use std::cell::Cell;

    #[test]
    fn button_renders_focus_style() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);
        let focus = Style::new().bg(Color::Blue);

        let button = Button::new("run", "Run").focused_style(focus).focused(true);
        let layout = button.layout(
            Constraints::tight(Rect::new(0, 0, 8, 1).size()),
            &mut LayoutCx::new(),
        );
        button.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("[ Run ] "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 0))
                .map(|cell| cell.style),
            Some(focus)
        );
    }

    #[test]
    fn dialog_renders_body_and_actions() {
        let actions = vec![
            DialogAction::new("allow", "Allow"),
            DialogAction::new("deny", "Deny"),
        ];
        let state = Cell::new(DialogState { focused_action: 1 });
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 5));
        let mut frame = Frame::new(&mut buffer);
        let component = DialogComponent::new(
            "permission-dialog",
            Dialog::new("Permit action?", &actions)
                .panel(Panel::new().border(Border::ascii()).title("Permission")),
            &state,
        );
        let layout = component.layout(
            Constraints::tight(frame.area().size()),
            &mut LayoutCx::new(),
        );
        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("+Permission--------+")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("|Permit action?    |")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("|[ Allow ] [ Deny ]|")
        );
        assert_eq!(state.get().focused_action, 1);
    }

    #[test]
    fn dialog_state_cycles_actions() {
        let mut state = DialogState::default();

        state.focus_next(2);
        assert_eq!(state.focused_action, 1);
        state.focus_next(2);
        assert_eq!(state.focused_action, 0);
        state.focus_previous(2);
        assert_eq!(state.focused_action, 1);
    }
}
