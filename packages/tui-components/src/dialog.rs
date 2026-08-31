//! Configurable dialog composition built from modal frame, action row, and text primitives.

use std::cell::Cell;

use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId,
    LayoutNode, LogicalSize,
};
use bmux_tui::composition::TextContent;
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::focus::FocusScopeId;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Text};

use crate::action_row::{
    ActionButton, ActionRow, ActionRowComponent, ActionRowOutcome, ActionRowState,
};
use crate::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing, ModalTheme,
};

/// Runtime dialog state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogState {
    /// Action row state for dialog actions.
    pub actions: ActionRowState,
}

impl DialogState {
    /// Create enabled dialog state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            actions: ActionRowState::new(),
        }
    }
}

impl Default for DialogState {
    fn default() -> Self {
        Self::new()
    }
}

/// Areas produced by [`Dialog::layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogLayout {
    /// Resolved modal panel area.
    pub panel: Rect,
    /// Modal content area.
    pub content: Rect,
    /// Body text area.
    pub body: Rect,
    /// Action row area.
    pub actions: Rect,
}

/// Outcome from dialog input handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without activating an action.
    Redraw,
    /// Dialog action was activated.
    Action { index: usize, id: String },
}

/// Canonical child-owning dialog component.
pub struct DialogComponent<'a, 'state> {
    id: LayoutId,
    dialog: Dialog<'a>,
    state: &'state Cell<ActionRowState>,
}

impl<'a, 'state> DialogComponent<'a, 'state> {
    /// Create a dialog component with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        dialog: Dialog<'a>,
        state: &'state Cell<ActionRowState>,
    ) -> Self {
        Self {
            id: id.into(),
            dialog,
            state,
        }
    }
}

struct DialogContent<'a, 'state> {
    id: LayoutId,
    body: Element<'a>,
    actions: Option<Element<'a>>,
    _state: std::marker::PhantomData<&'state ()>,
}

impl<'a, 'state: 'a> DialogContent<'a, 'state> {
    fn new(id: &LayoutId, dialog: &Dialog<'a>, state: &'state Cell<ActionRowState>) -> Self {
        let body = TextContent::new(Text::from_lines(dialog.body.to_vec()))
            .id(format!("{}.body", id.as_str()));
        let actions = (!dialog.actions.is_empty()).then(|| {
            Element::new(
                ActionRowComponent::new(format!("{}.actions", id.as_str()), dialog.actions, state)
                    .spacing(dialog.action_spacing),
            )
        });
        Self {
            id: LayoutId::new(format!("{}.content", id.as_str())),
            body: Element::new(body),
            actions,
            _state: std::marker::PhantomData,
        }
    }
}

impl Component for DialogContent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        self.body.revision().combine(
            self.actions
                .as_ref()
                .map_or_else(ComponentRevision::default, Element::revision),
        )
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let action_height = usize::from(self.actions.is_some());
        let body = self.body.layout(
            Constraints::new(
                constraints.min_width(),
                constraints.max_width(),
                constraints.min_height().saturating_sub(action_height),
                constraints
                    .max_height()
                    .map(|height| height.saturating_sub(action_height)),
            ),
            cx,
        );
        let width = body.size.width.max(constraints.min_width());
        let mut children = vec![ChildLayout::new(0, 0, body)];
        if let Some(actions) = &self.actions {
            let actions = actions.layout(Constraints::tight(Size::new(width, 1)), cx);
            let y = children[0].node.size.height;
            children.push(ChildLayout::new(0, y, actions));
        }
        let height = children
            .iter()
            .map(|child| child.y.saturating_add(child.node.size.height))
            .max()
            .unwrap_or(0);
        LayoutNode::with_children(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
            children,
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (index, child) in layout.children.iter().enumerate() {
            let component = if index == 0 {
                &self.body
            } else if let Some(actions) = &self.actions {
                actions
            } else {
                continue;
            };
            cx.with_child(
                i32::from(child.x),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                LocalRect::new(
                    0,
                    0,
                    child.node.size.width,
                    u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
                ),
                |cx| component.paint(&child.node, cx),
            );
        }
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(actions) = &self.actions else {
            return EventOutcome::Ignored;
        };
        let Some(child) = layout.children.get(1) else {
            return EventOutcome::Ignored;
        };
        let clip = Rect::new(
            child.x,
            u16::try_from(child.y).unwrap_or(u16::MAX),
            child.node.size.width,
            u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
        );
        cx.with_transform(
            child.x,
            child.y,
            i32::from(child.x),
            i64::try_from(child.y).unwrap_or(i64::MAX),
            clip,
            |cx| actions.event(event, &child.node, cx),
        )
    }
}

impl Component for DialogComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        self.tree().revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.tree().layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.tree().paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.tree().event(event, layout, cx)
    }
}

impl<'a, 'state: 'a> DialogComponent<'a, 'state> {
    fn tree(&self) -> ModalFrameComponent<'_> {
        ModalFrameComponent::new(
            self.id.clone(),
            self.dialog.modal(),
            DialogContent::new(&self.id, &self.dialog, self.state),
        )
    }
}

/// Modal dialog with body text and optional action row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog<'a> {
    title: Option<Line>,
    body: &'a [Line],
    actions: &'a [ActionButton],
    sizing: ModalSizing,
    theme: ModalTheme,
    placement: ModalPlacement,
    padding: Insets,
    action_spacing: u16,
}

impl<'a> Dialog<'a> {
    /// Create a dialog over caller-owned body lines and actions.
    #[must_use]
    pub const fn new(body: &'a [Line], actions: &'a [ActionButton], theme: ModalTheme) -> Self {
        Self {
            title: None,
            body,
            actions,
            sizing: ModalSizing::new(Size::new(20, 5), Size::new(80, 24), Insets::all(2)),
            theme,
            placement: ModalPlacement::Centered,
            padding: Insets::all(1),
            action_spacing: 1,
        }
    }

    /// Set dialog title.
    #[must_use]
    pub fn title(mut self, title: impl Into<Line>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set modal sizing.
    #[must_use]
    pub const fn sizing(mut self, sizing: ModalSizing) -> Self {
        self.sizing = sizing;
        self
    }

    /// Set modal placement.
    #[must_use]
    pub const fn placement(mut self, placement: ModalPlacement) -> Self {
        self.placement = placement;
        self
    }

    /// Set modal content padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    /// Set action spacing.
    #[must_use]
    pub const fn action_spacing(mut self, spacing: u16) -> Self {
        self.action_spacing = spacing;
        self
    }

    /// Return resolved dialog layout for a parent area.
    #[must_use]
    pub fn layout(&self, parent: Rect) -> DialogLayout {
        let modal = self.modal();
        let panel = modal.panel_area(parent);
        let content = modal.content_area(parent);
        let actions = if self.actions.is_empty() || content.height == 0 {
            Rect::new(content.x, content.bottom(), content.width, 0)
        } else {
            Rect::new(
                content.x,
                content.bottom().saturating_sub(1),
                content.width,
                1,
            )
        };
        let body_height = content.height.saturating_sub(actions.height);
        let body = Rect::new(content.x, content.y, content.width, body_height);
        DialogLayout {
            panel,
            content,
            body,
            actions,
        }
    }

    /// Render the dialog frame, body, and actions.
    pub fn render(&self, parent: Rect, state: &DialogState, frame: &mut Frame<'_>) {
        self.render_with_scope("dialog", parent, state, frame);
    }

    /// Render a modal focus scope and register only its visible action targets.
    pub fn render_with_scope(
        &self,
        scope: impl Into<FocusScopeId>,
        parent: Rect,
        state: &DialogState,
        frame: &mut Frame<'_>,
    ) {
        let scope = scope.into();
        frame.set_focus_scope(Some(scope.clone()));
        let modal = self.modal();
        modal.render(parent, frame);
        let layout = self.layout(parent);
        for (row, line) in self
            .body
            .iter()
            .take(usize::from(layout.body.height))
            .enumerate()
        {
            modal.render_line(
                Rect::new(
                    layout.body.x,
                    layout
                        .body
                        .y
                        .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                    layout.body.width,
                    1,
                ),
                line,
                frame,
            );
        }
        if !self.actions.is_empty() {
            let row = ActionRow::new(self.actions).spacing(self.action_spacing);
            row.render_state_with_id_prefix(layout.actions, &state.actions, frame, scope.as_str());
        }
    }

    /// Handle one input event by delegating to dialog actions.
    pub fn handle_event(
        &self,
        parent: Rect,
        state: &mut DialogState,
        event: &bmux_tui::event::Event,
    ) -> DialogOutcome {
        if self.actions.is_empty() {
            return DialogOutcome::Ignored;
        }
        if state.actions.focused().is_none() {
            state.actions.set_focused(Some(0));
        }
        match ActionRow::new(self.actions)
            .spacing(self.action_spacing)
            .handle_event(self.layout(parent).actions, &mut state.actions, event)
        {
            ActionRowOutcome::Ignored | ActionRowOutcome::Handled => DialogOutcome::Ignored,
            ActionRowOutcome::Redraw
            | ActionRowOutcome::FocusRequested { .. }
            | ActionRowOutcome::FocusMoved { .. } => DialogOutcome::Redraw,
            ActionRowOutcome::Activated { index, id } => DialogOutcome::Action { index, id },
        }
    }

    fn modal(&self) -> ModalFrame {
        let mut modal = ModalFrame::new(self.sizing, self.theme)
            .placement(self.placement)
            .padding(self.padding);
        if let Some(title) = self.title.clone() {
            modal = modal.title(title);
        }
        modal
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::event::Event;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Rect, Size};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::prelude::Line;
    use bmux_tui::style::Color;

    use crate::action_row::{ActionButton, ActionRowState};
    use crate::modal_frame::{ModalSizing, ModalTheme};

    use super::{Dialog, DialogComponent, DialogOutcome, DialogState};

    #[test]
    fn component_composes_modal_body_actions_and_routes_events() {
        let body = vec![Line::from("Proceed?")];
        let actions = vec![ActionButton::new("ok", "OK")];
        let state = Cell::new(ActionRowState::new());
        let dialog = Dialog::new(&body, &actions, ModalTheme::dark(Color::Cyan))
            .title("Confirm")
            .sizing(ModalSizing::fixed(Size::new(20, 7), Insets::all(0)));
        let component = DialogComponent::new("confirm", dialog, &state);
        let layout = component.layout(Constraints::new(30, 30, 10, Some(10)), &mut LayoutCx::new());
        assert!(layout.find(&"confirm.surface".into()).is_some());
        assert!(layout.find(&"confirm.body".into()).is_some());
        assert!(layout.find(&"confirm.actions".into()).is_some());

        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 10));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!((0..10).any(|row| {
            frame
                .buffer()
                .row_symbols(row)
                .is_some_and(|symbols| symbols.contains("Proceed?"))
        }));
        assert!((0..10).any(|row| {
            frame
                .buffer()
                .row_symbols(row)
                .is_some_and(|symbols| symbols.contains("[ OK ]"))
        }));

        assert!(
            component
                .event(
                    &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                    &layout,
                    &mut EventCx::new(&layout),
                )
                .is_handled()
        );
    }

    #[test]
    fn layout_reserves_last_content_row_for_actions() {
        let body = vec![Line::from("Delete this item?")];
        let actions = vec![ActionButton::new("ok", "OK")];
        let dialog = Dialog::new(&body, &actions, ModalTheme::dark(Color::Cyan)).sizing(
            ModalSizing::new(Size::new(20, 7), Size::new(20, 7), Insets::all(0)),
        );

        let layout = dialog.layout(Rect::new(0, 0, 30, 10));

        assert_eq!(layout.content, Rect::new(7, 3, 16, 3));
        assert_eq!(layout.body, Rect::new(7, 3, 16, 2));
        assert_eq!(layout.actions, Rect::new(7, 5, 16, 1));
    }

    #[test]
    fn renders_body_and_actions() {
        let body = vec![Line::from("Proceed?")];
        let actions = vec![ActionButton::new("ok", "OK")];
        let dialog = Dialog::new(&body, &actions, ModalTheme::dark(Color::Cyan)).sizing(
            ModalSizing::new(Size::new(20, 7), Size::new(20, 7), Insets::all(0)),
        );
        let mut state = DialogState::new();
        state.actions.set_focused(Some(0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 10));
        let mut frame = Frame::new(&mut buffer);

        dialog.render(frame.area(), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("     │ Proceed?         │     ")
        );
        assert_eq!(
            frame.buffer().row_symbols(5).as_deref(),
            Some("     │ [ OK ]           │     ")
        );
    }

    #[test]
    fn explicit_scope_tags_each_dialog_action_and_traps_focus() {
        let body = vec![Line::from("Proceed?")];
        let actions = vec![
            ActionButton::new("ok", "OK"),
            ActionButton::new("cancel", "Cancel"),
        ];
        let dialog = Dialog::new(&body, &actions, ModalTheme::dark(Color::Cyan)).sizing(
            ModalSizing::new(Size::new(24, 7), Size::new(24, 7), Insets::all(0)),
        );
        let state = DialogState::new();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 10));
        let mut frame = Frame::new(&mut buffer);

        dialog.render_with_scope("confirm", frame.area(), &state, &mut frame);

        assert_eq!(
            frame.focus_scope().map(bmux_tui::hit::HitId::as_str),
            Some("confirm")
        );
        assert_eq!(
            frame
                .hits()
                .regions()
                .iter()
                .map(|region| (
                    region.id.as_str(),
                    region
                        .focus_scope
                        .as_ref()
                        .map(bmux_tui::hit::HitId::as_str)
                ))
                .collect::<Vec<_>>(),
            vec![
                ("confirm.ok", Some("confirm")),
                ("confirm.cancel", Some("confirm")),
            ]
        );
    }

    #[test]
    fn rendered_dialog_scene_traps_pointer_and_restores_background_focus() {
        use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        use bmux_tui::interaction::InteractionRouter;

        let body = vec![Line::from("Proceed?")];
        let actions = vec![
            ActionButton::new("ok", "OK"),
            ActionButton::new("cancel", "Cancel"),
        ];
        let dialog = Dialog::new(&body, &actions, ModalTheme::dark(Color::Cyan));
        let state = DialogState::new();
        let background =
            bmux_tui::hit::HitRegion::new("background.action", Rect::new(0, 0, 30, 10))
                .focusable(true);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 10));
        let mut frame = Frame::new(&mut buffer);
        frame.push_hit(background.clone());
        dialog.render_with_scope("confirm", Rect::new(0, 0, 30, 10), &state, &mut frame);
        let scene = frame.hits().clone();
        let mut router = InteractionRouter::new();

        router.commit_scene(
            bmux_tui::hit::HitMap::new().with_region(background.clone()),
            None,
        );
        assert_eq!(
            router.focused().map(bmux_tui::hit::HitId::as_str),
            Some("background.action")
        );
        router.commit_scene(scene, Some(bmux_tui::hit::HitId::new("confirm")));
        assert_eq!(
            router.focused().map(bmux_tui::hit::HitId::as_str),
            Some("confirm.ok")
        );
        assert_eq!(
            router
                .route(Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(0, 0),
                )))
                .target,
            None
        );
        router.commit_scene(bmux_tui::hit::HitMap::new().with_region(background), None);
        assert_eq!(
            router.focused().map(bmux_tui::hit::HitId::as_str),
            Some("background.action")
        );
    }

    #[test]
    fn action_activation_returns_action_outcome() {
        let body = vec![Line::from("Proceed?")];
        let actions = vec![ActionButton::new("ok", "OK")];
        let dialog = Dialog::new(&body, &actions, ModalTheme::dark(Color::Cyan)).sizing(
            ModalSizing::new(Size::new(20, 7), Size::new(20, 7), Insets::all(0)),
        );
        let mut state = DialogState::new();
        state.actions.set_focused(Some(0));

        let outcome = dialog.handle_event(
            Rect::new(0, 0, 30, 10),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(
            outcome,
            DialogOutcome::Action {
                index: 0,
                id: "ok".to_string()
            }
        );
    }
}
