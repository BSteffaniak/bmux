//! Higher-level text-input control with opt-in behavior policies.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::input::TextInput;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::Style;
use unicode_segmentation::UnicodeSegmentation;

use crate::scroll_view::{ScrollView, ScrollViewComponent, ScrollViewState};

const DEFAULT_MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);
const DEFAULT_MULTI_CLICK_DISTANCE: u16 = 2;

/// Stateful text input data used by [`TextInputControl`].
#[derive(Debug, Clone)]
pub struct TextInputState {
    buffer: TextEditBuffer,
    content_area: Rect,
    scroll: ScrollViewState,
    mouse_selection: MouseSelectionState,
    wrapped: RefCell<Option<(u16, usize, bmux_text_edit::WrapLayout)>>,
}

// Derived measurement state must not change the meaning of editor equality.
impl PartialEq for TextInputState {
    fn eq(&self, other: &Self) -> bool {
        self.buffer == other.buffer
            && self.content_area == other.content_area
            && self.scroll == other.scroll
            && self.mouse_selection == other.mouse_selection
    }
}

impl Eq for TextInputState {}

impl Default for TextInputState {
    fn default() -> Self {
        Self::new(TextEditBuffer::new())
    }
}

impl TextInputState {
    /// Create state around an edit buffer.
    #[must_use]
    pub fn new(buffer: TextEditBuffer) -> Self {
        Self {
            buffer,
            content_area: Rect::new(0, 0, 1, 1),
            scroll: ScrollViewState::new(),
            mouse_selection: MouseSelectionState::default(),
            wrapped: RefCell::new(None),
        }
    }

    fn wrapped_layout(&self, width: u16) -> std::cell::Ref<'_, bmux_text_edit::WrapLayout> {
        let stale = self
            .wrapped
            .borrow()
            .as_ref()
            .is_none_or(|(cached_width, _, _)| *cached_width != width);
        if stale {
            *self.wrapped.borrow_mut() = Some((
                width,
                self.buffer.cursor_byte_index(),
                self.buffer.wrapped_layout(usize::from(width.max(1))),
            ));
        } else if let Some((_, cursor, layout)) = self.wrapped.borrow_mut().as_mut()
            && *cursor != self.buffer.cursor_byte_index()
        {
            *cursor = self.buffer.cursor_byte_index();
            layout.cursor = layout.cursor_for_byte_index(self.buffer.text(), *cursor);
        }
        std::cell::Ref::map(self.wrapped.borrow(), |cached| {
            &cached.as_ref().expect("measured projection").2
        })
    }

    /// Return the edit buffer.
    #[must_use]
    pub const fn buffer(&self) -> &TextEditBuffer {
        &self.buffer
    }

    /// Return the mutable edit buffer, releasing derived layout before arbitrary edits.
    pub fn buffer_mut(&mut self) -> &mut TextEditBuffer {
        *self.wrapped.get_mut() = None;
        &mut self.buffer
    }

    /// Return the latest content area.
    #[must_use]
    pub const fn content_area(&self) -> Rect {
        self.content_area
    }

    /// Return the vertical viewport scroll in wrapped rows.
    #[must_use]
    pub const fn vertical_scroll(&self) -> usize {
        self.scroll.vertical_offset()
    }

    /// Store the latest content area.
    pub fn set_content_area(&mut self, area: Rect, policy: &TextInputPolicy) {
        self.content_area = area;
        self.sync_scroll_to_cursor(policy);
        let rows = self.wrapped_layout(area.width).lines.len();
        let viewport = editor_viewport(area, rows);
        ScrollView::scroll_vertical_by(&viewport, &mut self.scroll, 0);
    }

    /// Synchronize vertical scroll so the cursor is visible if policy allows.
    pub fn sync_scroll_to_cursor(&mut self, policy: &TextInputPolicy) {
        let Some(offset) = self.cursor_scroll_offset(policy) else {
            return;
        };
        self.scroll.set_vertical_offset(offset);
    }

    /// Return the scroll offset that keeps the cursor visible.
    #[must_use]
    pub fn cursor_scroll_offset(&self, policy: &TextInputPolicy) -> Option<usize> {
        if !policy.viewport.auto_scroll_to_cursor || self.content_area.height == 0 {
            return None;
        }
        let layout = self.wrapped_layout(self.content_area.width);
        let viewport = editor_viewport(
            self.content_area,
            layout.lines.len().max(layout.cursor.row.saturating_add(1)),
        );
        // Preserve the editor's cursor-at-bottom policy while sharing reveal/clamping.
        let mut scroll = ScrollViewState::new();
        ScrollView::new().ensure_visible(&viewport, &mut scroll, layout.cursor.row, 1);
        Some(scroll.vertical_offset())
    }

    /// Return whether a mouse selection drag is active.
    #[must_use]
    pub const fn mouse_selection_active(&self) -> bool {
        !matches!(self.mouse_selection.active, SelectionGranularity::Disabled)
    }
}

/// Canonical component-lifecycle editable text leaf.
///
/// The edit buffer, selection, scroll, and pointer gesture state remain
/// caller-owned through the supplied `RefCell`.
pub struct TextInputComponent<'policy, 'state> {
    id: LayoutId,
    state: &'state RefCell<TextInputState>,
    policy: &'policy TextInputPolicy,
    style: Style,
    selection_style: Style,
    placeholder: Option<&'policy str>,
    placeholder_style: Style,
    focused: bool,
    disabled: bool,
}

impl<'policy, 'state> TextInputComponent<'policy, 'state> {
    /// Create an editable text component with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        state: &'state RefCell<TextInputState>,
        policy: &'policy TextInputPolicy,
    ) -> Self {
        Self {
            id: id.into(),
            state,
            policy,
            style: Style::new(),
            selection_style: Style::new().add_modifier(bmux_tui::style::Modifier::REVERSED),
            placeholder: None,
            placeholder_style: Style::new(),
            focused: false,
            disabled: false,
        }
    }

    /// Set rendered text style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set selected text style.
    #[must_use]
    pub const fn selection_style(mut self, style: Style) -> Self {
        self.selection_style = style;
        self
    }

    /// Set placeholder text and style.
    #[must_use]
    pub const fn placeholder(mut self, text: &'policy str, style: Style) -> Self {
        self.placeholder = Some(text);
        self.placeholder_style = style;
        self
    }

    /// Set focus presentation and cursor visibility.
    #[must_use]
    pub const fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Disable editing and pointer interaction.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl Component for TextInputComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let state = self.state.borrow();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        state.buffer().text().hash(&mut layout);
        self.policy.viewport.min_rows.hash(&mut layout);
        self.policy.viewport.max_rows.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        state.buffer().cursor_grapheme_index().hash(&mut paint);
        if let Some(selection) = state.buffer().selection() {
            selection.start.hash(&mut paint);
            selection.end.hash(&mut paint);
        }
        state.vertical_scroll().hash(&mut paint);
        self.style.hash(&mut paint);
        self.selection_style.hash(&mut paint);
        self.placeholder.hash(&mut paint);
        self.placeholder_style.hash(&mut paint);
        self.focused.hash(&mut paint);
        self.disabled.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = constraints.max_width();
        let state = self.state.borrow();
        let rows = TextInputControl::new(self.policy).visible_rows_for_width(&state, width);
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, usize::from(rows))),
        )
        .with_metadata(LayoutMetadata::new().semantic("text-input"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let local = LocalRect::new(0, 0, layout.size.width, height);
        let area = Rect::new(0, 0, layout.size.width, height);
        let mut state = self.state.borrow_mut();
        state.set_content_area(area, self.policy);
        let wrapped = state.wrapped_layout(area.width);
        let mut input = TextInput::new(state.buffer())
            .wrapped_layout(&wrapped)
            .id(self.id.clone())
            .style(self.style)
            .selection_style(self.selection_style)
            .placeholder_style(self.placeholder_style)
            .cursor_visible(self.focused && !self.disabled)
            .vertical_scroll(state.vertical_scroll());
        if let Some(placeholder) = self.placeholder {
            input = input.placeholder(placeholder);
        }
        input.paint(layout, cx);
        cx.push_hit(
            HitRegion::new(self.id.as_str(), area)
                .role(HitRole::TextInput)
                .hoverable(true)
                .focusable(true)
                .enabled(!self.disabled),
        );
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "text-input"));
        cx.push_damage(local);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        if self.disabled {
            return EventOutcome::Ignored;
        }
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.borrow_mut();
        state.set_content_area(area, self.policy);
        match TextInputControl::new(self.policy).handle_event(&mut state, event) {
            TextInputOutcome::Ignored => EventOutcome::Ignored,
            TextInputOutcome::Edited
            | TextInputOutcome::Redraw
            | TextInputOutcome::Submitted
            | TextInputOutcome::EdgeUp
            | TextInputOutcome::EdgeDown => EventOutcome::Redraw,
        }
    }
}

/// Stateless text-input event controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputControl<'policy> {
    policy: &'policy TextInputPolicy,
}

impl<'policy> TextInputControl<'policy> {
    /// Create a control using `policy`.
    #[must_use]
    pub const fn new(policy: &'policy TextInputPolicy) -> Self {
        Self { policy }
    }

    /// Return the configured policy.
    #[must_use]
    pub const fn policy(&self) -> &TextInputPolicy {
        self.policy
    }

    /// Return visible content rows for a terminal width.
    #[must_use]
    pub fn visible_rows_for_width(&self, state: &TextInputState, width: u16) -> u16 {
        let wrapped_rows = state.wrapped_layout(width).lines.len().max(1);
        usize_to_u16_saturating(wrapped_rows)
            .max(self.policy.viewport.min_rows.max(1))
            .min(self.policy.viewport.max_rows.unwrap_or(u16::MAX))
    }

    /// Handle one input event.
    pub fn handle_event(&self, state: &mut TextInputState, event: &Event) -> TextInputOutcome {
        match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(state, *mouse),
            Event::Paste(text) => self.handle_paste(state, text),
            Event::Resize(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                TextInputOutcome::Ignored
            }
        }
    }

    /// Handle bracketed pasted text.
    pub fn handle_paste(&self, state: &mut TextInputState, text: &str) -> TextInputOutcome {
        state.buffer_mut().paste(text);
        if self.policy.viewport.auto_scroll_to_cursor {
            state.scroll.set_vertical_offset(usize::MAX);
        }
        TextInputOutcome::Edited
    }

    /// Handle one keyboard stroke.
    pub fn handle_key(&self, state: &mut TextInputState, stroke: KeyStroke) -> TextInputOutcome {
        if !self.policy.keyboard.enabled {
            return TextInputOutcome::Ignored;
        }
        if let Some(outcome) = self.handle_enter(state, stroke) {
            return outcome;
        }
        if self.policy.keyboard.selection_keys
            && let Some(motion) = selection_motion(stroke)
        {
            extend_selection(state, motion);
            state.sync_scroll_to_cursor(self.policy);
            return TextInputOutcome::Edited;
        }
        if let Some(outcome) = self.handle_edge_key(state, stroke) {
            return outcome;
        }
        let Some(command) = self.policy.keyboard.keymap.command_for_key(stroke) else {
            return TextInputOutcome::Ignored;
        };
        if matches!(command, bmux_text_edit::TextEditCommand::Move(_)) {
            state.buffer.apply_command(command);
        } else {
            state.buffer_mut().apply_command(command);
        }
        state.sync_scroll_to_cursor(self.policy);
        TextInputOutcome::Edited
    }

    /// Handle one mouse event.
    pub fn handle_mouse(&self, state: &mut TextInputState, mouse: MouseEvent) -> TextInputOutcome {
        if !self.policy.mouse.enabled {
            return TextInputOutcome::Ignored;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click_to_cursor => {
                self.handle_mouse_down(state, mouse)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.policy.mouse.drag_selection => {
                self.handle_mouse_drag(state, mouse)
            }
            MouseEventKind::Up(MouseButton::Left) if state.mouse_selection_active() => {
                state.mouse_selection.active = SelectionGranularity::Disabled;
                TextInputOutcome::Redraw
            }
            MouseEventKind::Down(
                MouseButton::Left
                | MouseButton::Right
                | MouseButton::Middle
                | MouseButton::Other(_),
            )
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => TextInputOutcome::Ignored,
        }
    }

    fn handle_enter(
        &self,
        state: &mut TextInputState,
        stroke: KeyStroke,
    ) -> Option<TextInputOutcome> {
        if stroke.key != KeyCode::Enter {
            return None;
        }
        let behavior = if stroke.modifiers.shift {
            self.policy
                .keyboard
                .shift_enter
                .unwrap_or(self.policy.keyboard.enter)
        } else if stroke.modifiers.is_empty() {
            self.policy.keyboard.enter
        } else {
            return None;
        };
        Some(apply_enter_behavior(state, self.policy, behavior))
    }

    fn handle_edge_key(
        &self,
        state: &TextInputState,
        stroke: KeyStroke,
    ) -> Option<TextInputOutcome> {
        if !stroke.modifiers.is_empty() {
            return None;
        }
        let layout = state.wrapped_layout(state.content_area.width);
        match stroke.key {
            KeyCode::Up if layout.cursor.row == 0 && self.policy.edge.up_at_first_row => {
                Some(TextInputOutcome::EdgeUp)
            }
            KeyCode::Down
                if layout.cursor.row.saturating_add(1) >= layout.lines.len()
                    && self.policy.edge.down_at_last_row =>
            {
                Some(TextInputOutcome::EdgeDown)
            }
            KeyCode::Char(_)
            | KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Escape
            | KeyCode::Space
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Insert
            | KeyCode::F(_) => None,
        }
    }

    fn handle_mouse_down(&self, state: &mut TextInputState, mouse: MouseEvent) -> TextInputOutcome {
        let Some((row, col)) = mouse_wrapped_position(state, mouse) else {
            state.mouse_selection.active = SelectionGranularity::Disabled;
            return TextInputOutcome::Ignored;
        };
        let count = state
            .mouse_selection
            .click_count(mouse.position.x, mouse.position.y);
        let granularity = self.policy.mouse.granularity_for_click_count(count);
        state.mouse_selection.active = if self.policy.mouse.drag_selection {
            granularity
        } else {
            SelectionGranularity::Disabled
        };
        let byte_index = state
            .wrapped_layout(state.content_area.width)
            .byte_index_for_position(row, col);
        apply_selection_granularity(&mut state.buffer, byte_index, granularity);
        state.sync_scroll_to_cursor(self.policy);
        TextInputOutcome::Redraw
    }

    fn handle_mouse_drag(&self, state: &mut TextInputState, mouse: MouseEvent) -> TextInputOutcome {
        let Some(position) = drag_wrapped_position(
            state,
            mouse,
            matches!(self.policy.mouse.edge_scroll, DragEdgeScroll::Enabled),
        ) else {
            return TextInputOutcome::Ignored;
        };
        let byte_index = state
            .wrapped_layout(state.content_area.width)
            .byte_index_for_position(position.row, position.col);
        extend_selection_to_granularity(
            &mut state.buffer,
            byte_index,
            state.mouse_selection.active,
        );
        if !position.scrolled {
            state.sync_scroll_to_cursor(self.policy);
        }
        TextInputOutcome::Redraw
    }
}

/// Whether editable text participates in an enclosing content-selection scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TextInputOuterSelectionPolicy {
    /// Editable text owns pointer selection and blocks outer content selection.
    #[default]
    Isolated,
    /// The containing application may register the input as delegated content.
    Delegate,
}

/// Configurable text-input behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInputPolicy {
    /// Participation in an outer logical content-selection scope.
    pub outer_selection: TextInputOuterSelectionPolicy,
    /// Keyboard behavior.
    pub keyboard: KeyboardPolicy,
    /// Mouse behavior.
    pub mouse: MousePolicy,
    /// Viewport behavior.
    pub viewport: ViewportPolicy,
    /// Edge signal behavior.
    pub edge: EdgePolicy,
}

impl Default for TextInputPolicy {
    fn default() -> Self {
        Self::raw()
    }
}

impl TextInputPolicy {
    /// Raw policy with all higher-level handling disabled.
    #[must_use]
    pub const fn raw() -> Self {
        Self {
            outer_selection: TextInputOuterSelectionPolicy::Isolated,
            keyboard: KeyboardPolicy::disabled(),
            mouse: MousePolicy::disabled(),
            viewport: ViewportPolicy::raw(),
            edge: EdgePolicy::disabled(),
        }
    }

    /// Return this policy with outer selection participation changed.
    #[must_use]
    pub const fn outer_selection(mut self, policy: TextInputOuterSelectionPolicy) -> Self {
        self.outer_selection = policy;
        self
    }

    /// Common chat-composer policy.
    #[must_use]
    pub const fn chat_composer() -> Self {
        Self {
            outer_selection: TextInputOuterSelectionPolicy::Isolated,
            keyboard: KeyboardPolicy::chat_composer(),
            mouse: MousePolicy::text_selection(),
            viewport: ViewportPolicy {
                auto_scroll_to_cursor: true,
                min_rows: 1,
                max_rows: Some(6),
            },
            edge: EdgePolicy {
                up_at_first_row: true,
                down_at_last_row: true,
            },
        }
    }
}

/// Keyboard behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardPolicy {
    /// Whether keyboard handling is enabled.
    pub enabled: bool,
    /// Standard edit keymap.
    pub keymap: TextKeymap,
    /// Enter key behavior.
    pub enter: EnterBehavior,
    /// Shift+Enter behavior.
    pub shift_enter: Option<EnterBehavior>,
    /// Whether shift-selection bindings are handled.
    pub selection_keys: bool,
}

impl KeyboardPolicy {
    /// Disabled keyboard handling.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            keymap: TextKeymap {
                profile: bmux_text_edit::keyboard::TextInputProfile::Readline,
                boundary_policy: bmux_text_edit::TextBoundaryPolicy::Buffer,
            },
            enter: EnterBehavior::Ignore,
            shift_enter: None,
            selection_keys: false,
        }
    }

    /// Common chat-composer keyboard handling.
    #[must_use]
    pub const fn chat_composer() -> Self {
        Self {
            enabled: true,
            keymap: TextKeymap {
                profile: bmux_text_edit::keyboard::TextInputProfile::Readline,
                boundary_policy: bmux_text_edit::TextBoundaryPolicy::Buffer,
            },
            enter: EnterBehavior::Submit,
            shift_enter: Some(EnterBehavior::InsertNewline),
            selection_keys: true,
        }
    }
}

/// Enter-key behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterBehavior {
    /// Do not handle enter.
    Ignore,
    /// Insert a newline into the buffer.
    InsertNewline,
    /// Emit a submit outcome.
    Submit,
}

/// Mouse behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePolicy {
    /// Whether mouse handling is enabled.
    pub enabled: bool,
    /// Whether clicks place the cursor/select text.
    pub click_to_cursor: bool,
    /// Whether dragging extends selection.
    pub drag_selection: bool,
    /// Whether dragging beyond the visible top/bottom scrolls the input viewport.
    pub edge_scroll: DragEdgeScroll,
    /// Double-click selection behavior.
    pub double_click: Option<SelectionGranularity>,
    /// Triple-click selection behavior.
    pub triple_click: Option<SelectionGranularity>,
}

/// Drag behavior when the mouse leaves the visible top or bottom edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragEdgeScroll {
    /// Ignore drag events outside the text input bounds.
    Disabled,
    /// Scroll the viewport and extend selection while dragging beyond edges.
    Enabled,
}

impl MousePolicy {
    /// Disabled mouse handling.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            click_to_cursor: false,
            drag_selection: false,
            edge_scroll: DragEdgeScroll::Disabled,
            double_click: None,
            triple_click: None,
        }
    }

    /// Text-selection mouse behavior.
    #[must_use]
    pub const fn text_selection() -> Self {
        Self {
            enabled: true,
            click_to_cursor: true,
            drag_selection: true,
            edge_scroll: DragEdgeScroll::Enabled,
            double_click: Some(SelectionGranularity::Word),
            triple_click: Some(SelectionGranularity::All),
        }
    }

    const fn granularity_for_click_count(self, count: u8) -> SelectionGranularity {
        match count {
            3.. => option_granularity_or(self.triple_click, SelectionGranularity::Character),
            2 => option_granularity_or(self.double_click, SelectionGranularity::Character),
            _ => SelectionGranularity::Character,
        }
    }
}

/// Viewport behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportPolicy {
    /// Whether viewport scroll follows the cursor.
    pub auto_scroll_to_cursor: bool,
    /// Minimum visible rows.
    pub min_rows: u16,
    /// Maximum visible rows.
    pub max_rows: Option<u16>,
}

impl ViewportPolicy {
    /// Raw viewport behavior.
    #[must_use]
    pub const fn raw() -> Self {
        Self {
            auto_scroll_to_cursor: false,
            min_rows: 1,
            max_rows: None,
        }
    }
}

/// Edge signal behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgePolicy {
    /// Emit [`TextInputOutcome::EdgeUp`] when up is pressed on the first row.
    pub up_at_first_row: bool,
    /// Emit [`TextInputOutcome::EdgeDown`] when down is pressed on the last row.
    pub down_at_last_row: bool,
}

impl EdgePolicy {
    /// Disabled edge signals.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            up_at_first_row: false,
            down_at_last_row: false,
        }
    }
}

/// Selection granularity for mouse actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionGranularity {
    /// Select by character/cell hit target.
    Character,
    /// Select whole words.
    Word,
    /// Select the entire buffer.
    All,
    /// Disable active selection extension.
    Disabled,
}

/// Outcome from handling input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputOutcome {
    /// Event was ignored.
    Ignored,
    /// Buffer/cursor/selection changed.
    Edited,
    /// Redraw requested without a text edit.
    Redraw,
    /// Submit was requested.
    Submitted,
    /// Up was pressed at the first visual row.
    EdgeUp,
    /// Down was pressed at the last visual row.
    EdgeDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MouseSelectionState {
    last_click: Option<MouseClickState>,
    active: SelectionGranularity,
}

impl Default for MouseSelectionState {
    fn default() -> Self {
        Self {
            last_click: None,
            active: SelectionGranularity::Disabled,
        }
    }
}

impl MouseSelectionState {
    fn click_count(&mut self, x: u16, y: u16) -> u8 {
        let now = Instant::now();
        let count = self.last_click.map_or(1, |last| {
            let near = last.x.abs_diff(x) <= DEFAULT_MULTI_CLICK_DISTANCE
                && last.y.abs_diff(y) <= DEFAULT_MULTI_CLICK_DISTANCE;
            let quick = now.saturating_duration_since(last.at) <= DEFAULT_MULTI_CLICK_WINDOW;
            if near && quick {
                last.count.saturating_add(1)
            } else {
                1
            }
        });
        let capped = count.min(3);
        self.last_click = Some(MouseClickState {
            x,
            y,
            at: now,
            count: capped,
        });
        capped
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseClickState {
    x: u16,
    y: u16,
    at: Instant,
    count: u8,
}

const fn option_granularity_or(
    value: Option<SelectionGranularity>,
    fallback: SelectionGranularity,
) -> SelectionGranularity {
    match value {
        Some(value) => value,
        None => fallback,
    }
}

fn apply_enter_behavior(
    state: &mut TextInputState,
    policy: &TextInputPolicy,
    behavior: EnterBehavior,
) -> TextInputOutcome {
    match behavior {
        EnterBehavior::Ignore => TextInputOutcome::Ignored,
        EnterBehavior::InsertNewline => {
            state.buffer_mut().insert_newline();
            state.sync_scroll_to_cursor(policy);
            TextInputOutcome::Edited
        }
        EnterBehavior::Submit => TextInputOutcome::Submitted,
    }
}

const fn selection_motion(stroke: KeyStroke) -> Option<TextMotion> {
    let Modifiers {
        ctrl,
        alt,
        shift,
        super_key,
        hyper,
        meta,
    } = stroke.modifiers;
    if !shift || super_key || hyper || meta {
        return None;
    }
    match stroke.key {
        KeyCode::Left if ctrl || alt => Some(TextMotion::WordLeft),
        KeyCode::Right if ctrl || alt => Some(TextMotion::WordRight),
        KeyCode::Left => Some(TextMotion::Left),
        KeyCode::Right => Some(TextMotion::Right),
        KeyCode::Up => Some(TextMotion::VisualUp),
        KeyCode::Down => Some(TextMotion::VisualDown),
        KeyCode::Char(_)
        | KeyCode::Enter
        | KeyCode::Tab
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Escape
        | KeyCode::Space
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Insert
        | KeyCode::F(_) => None,
    }
}

fn extend_selection(state: &mut TextInputState, motion: TextMotion) {
    match motion {
        TextMotion::VisualUp => extend_visual_selection(state, -1),
        TextMotion::VisualDown => extend_visual_selection(state, 1),
        motion => state
            .buffer
            .move_cursor_with_selection(motion, SelectionMode::Extend),
    }
}

fn extend_visual_selection(state: &mut TextInputState, delta: isize) {
    let width = state.content_area.width;
    let layout = state.wrapped_layout(width);
    let target_row = if delta.is_negative() {
        layout.cursor.row.saturating_sub(delta.unsigned_abs())
    } else {
        layout
            .cursor
            .row
            .saturating_add(delta.unsigned_abs())
            .min(layout.lines.len().saturating_sub(1))
    };
    let byte_index = layout.byte_index_for_position(target_row, layout.cursor.col);
    drop(layout);
    state
        .buffer
        .move_cursor_with_selection(TextMotion::Absolute(byte_index), SelectionMode::Extend);
}

fn mouse_wrapped_position(state: &TextInputState, mouse: MouseEvent) -> Option<(usize, usize)> {
    let area = state.content_area;
    if mouse.position.y < area.y || mouse.position.y >= area.bottom() {
        return None;
    }
    if mouse.position.x < area.x || mouse.position.x >= area.right() {
        return None;
    }
    Some((
        usize::from(mouse.position.y.saturating_sub(area.y))
            .saturating_add(state.vertical_scroll()),
        usize::from(mouse.position.x.saturating_sub(area.x)),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DragPosition {
    row: usize,
    col: usize,
    scrolled: bool,
}

fn drag_wrapped_position(
    state: &mut TextInputState,
    mouse: MouseEvent,
    edge_scroll: bool,
) -> Option<DragPosition> {
    let area = state.content_area;
    if area.is_empty() {
        return None;
    }
    let col = clamped_mouse_col(area, mouse.position.x);
    if mouse.position.y < area.y {
        if !edge_scroll {
            return None;
        }
        let previous = state.vertical_scroll();
        let viewport = editor_viewport(area, state.wrapped_layout(area.width).lines.len());
        ScrollView::scroll_vertical_by(&viewport, &mut state.scroll, -1);
        return Some(DragPosition {
            row: state.vertical_scroll(),
            col,
            scrolled: state.vertical_scroll() != previous,
        });
    }
    if mouse.position.y >= area.bottom() {
        if !edge_scroll {
            return None;
        }
        let previous = state.vertical_scroll();
        let viewport = editor_viewport(area, state.wrapped_layout(area.width).lines.len());
        ScrollView::scroll_vertical_by(&viewport, &mut state.scroll, 1);
        return Some(DragPosition {
            row: state
                .vertical_scroll()
                .saturating_add(usize::from(area.height).saturating_sub(1)),
            col,
            scrolled: state.vertical_scroll() != previous,
        });
    }
    Some(DragPosition {
        row: usize::from(mouse.position.y.saturating_sub(area.y))
            .saturating_add(state.vertical_scroll()),
        col,
        scrolled: false,
    })
}

fn clamped_mouse_col(area: Rect, x: u16) -> usize {
    if x < area.x {
        0
    } else if x >= area.right() {
        usize::from(area.width.saturating_sub(1))
    } else {
        usize::from(x.saturating_sub(area.x))
    }
}

fn editor_viewport(area: Rect, rows: usize) -> LayoutNode {
    ScrollViewComponent::viewport_layout(
        LayoutId::new("text-input.viewport"),
        LogicalSize::new(area.width, usize::from(area.height)),
        LayoutNode::leaf(
            LayoutId::new("text-input.content"),
            LogicalSize::new(area.width, rows),
        ),
    )
}

fn apply_selection_granularity(
    buffer: &mut TextEditBuffer,
    byte_index: usize,
    granularity: SelectionGranularity,
) {
    match granularity {
        SelectionGranularity::Character | SelectionGranularity::Disabled => {
            buffer.move_cursor(TextMotion::Absolute(byte_index));
        }
        SelectionGranularity::Word => select_word_at(buffer, byte_index),
        SelectionGranularity::All => buffer.select_all(),
    }
}

fn extend_selection_to_granularity(
    buffer: &mut TextEditBuffer,
    byte_index: usize,
    granularity: SelectionGranularity,
) {
    match granularity {
        SelectionGranularity::Character => {
            buffer.move_cursor_with_selection(
                TextMotion::Absolute(byte_index),
                SelectionMode::Extend,
            );
        }
        SelectionGranularity::Word => {
            let target =
                word_range_at(buffer.text(), byte_index).map_or(byte_index, |(_, end)| end);
            buffer.move_cursor_with_selection(TextMotion::Absolute(target), SelectionMode::Extend);
        }
        SelectionGranularity::All => buffer.select_all(),
        SelectionGranularity::Disabled => {}
    }
}

fn select_word_at(buffer: &mut TextEditBuffer, byte_index: usize) {
    if let Some((start, end)) = word_range_at(buffer.text(), byte_index) {
        buffer.move_cursor(TextMotion::Absolute(start));
        buffer.move_cursor_with_selection(TextMotion::Absolute(end), SelectionMode::Extend);
    } else {
        buffer.move_cursor(TextMotion::Absolute(byte_index));
    }
}

fn word_range_at(text: &str, byte_index: usize) -> Option<(usize, usize)> {
    if text.is_empty() {
        return None;
    }
    let index = byte_index.min(text.len());
    let ranges = text
        .grapheme_indices(true)
        .map(|(start, grapheme)| (start, start.saturating_add(grapheme.len()), grapheme))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return None;
    }
    let mut position = ranges
        .iter()
        .position(|(start, end, _)| index >= *start && index < *end)
        .unwrap_or_else(|| ranges.len().saturating_sub(1));
    if ranges[position].2.chars().all(char::is_whitespace) {
        position = ranges
            .iter()
            .enumerate()
            .skip(position)
            .find(|(_, (_, _, grapheme))| !grapheme.chars().all(char::is_whitespace))
            .map_or(position, |(index, _)| index);
    }
    if ranges[position].2.chars().all(char::is_whitespace) {
        return None;
    }
    let mut start_position = position;
    while start_position > 0 && is_word_grapheme(ranges[start_position - 1].2) {
        start_position -= 1;
    }
    let mut end_position = position;
    while end_position + 1 < ranges.len() && is_word_grapheme(ranges[end_position + 1].2) {
        end_position += 1;
    }
    Some((ranges[start_position].0, ranges[end_position].1))
}

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(|ch| ch.is_alphanumeric() || ch == '_')
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Size};
    use bmux_tui::paint::{LocalRect, PaintCx};

    fn key(key: KeyCode) -> KeyStroke {
        KeyStroke::simple(key)
    }

    fn shift_key(key: KeyCode) -> KeyStroke {
        KeyStroke::with_modifiers(
            key,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        )
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent::new(kind, Point::new(x, y))
    }

    #[test]
    fn wrapped_cache_reuses_and_invalidates_without_affecting_equality() {
        let mut state = TextInputState::new(TextEditBuffer::from_text("abcdef"));
        let unchanged = state.clone();
        let first = state.wrapped_layout(3).lines.as_ptr();
        assert_eq!(first, state.wrapped_layout(3).lines.as_ptr());
        assert_eq!(state, unchanged);
        assert_eq!(*state.wrapped_layout(2), state.buffer().wrapped_layout(2));
        state.buffer_mut().move_cursor(TextMotion::Left);
        assert_eq!(*state.wrapped_layout(2), state.buffer().wrapped_layout(2));
        *state.buffer_mut() = TextEditBuffer::from_text("uvwxyz");
        assert_eq!(*state.wrapped_layout(2), state.buffer().wrapped_layout(2));
        assert_eq!(state.wrapped_layout(2).lines[0], "uv");
    }

    #[test]
    fn input_events_retain_cursor_geometry_and_invalidate_text_geometry() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello 界world"));
        state.set_content_area(Rect::new(0, 0, 5, 2), &policy);
        let lines = state.wrapped_layout(5).lines.as_ptr();
        for stroke in [
            key(KeyCode::Left),
            shift_key(KeyCode::Left),
            shift_key(KeyCode::Up),
        ] {
            control.handle_key(&mut state, stroke);
            assert_eq!(state.wrapped_layout(5).lines.as_ptr(), lines);
            assert_eq!(*state.wrapped_layout(5), state.buffer().wrapped_layout(5));
        }
        for event in [
            Event::Key(key(KeyCode::Char('!'))),
            Event::Key(key(KeyCode::Backspace)),
            Event::Paste("long pasted 界text".to_string()),
            Event::Key(shift_key(KeyCode::Enter)),
        ] {
            let previous = state.buffer().text().to_string();
            control.handle_event(&mut state, &event);
            assert_ne!(state.buffer().text(), previous);
            assert_eq!(*state.wrapped_layout(5), state.buffer().wrapped_layout(5));
        }
        *state.buffer_mut() = TextEditBuffer::from_text("replacement");
        assert!(state.wrapped.borrow().is_none());
        assert_eq!(*state.wrapped_layout(5), state.buffer().wrapped_layout(5));
        let cloned = state.clone();
        state.buffer_mut().clear();
        assert_eq!(*state.wrapped_layout(5), state.buffer().wrapped_layout(5));
        assert_eq!(*cloned.wrapped_layout(5), cloned.buffer().wrapped_layout(5));
    }

    #[test]
    fn resize_and_content_replacement_clamp_manual_scroll() {
        let mut policy = TextInputPolicy::chat_composer();
        policy.viewport.auto_scroll_to_cursor = false;
        let mut state = TextInputState::new(TextEditBuffer::from_text("0\n1\n2\n3\n4"));
        state.scroll.set_vertical_offset(3);
        state.set_content_area(Rect::new(0, 0, 4, 4), &policy);
        assert_eq!(state.vertical_scroll(), 1);
        *state.buffer_mut() = TextEditBuffer::from_text("short");
        state.set_content_area(Rect::new(0, 0, 8, 4), &policy);
        assert_eq!(state.vertical_scroll(), 0);
    }

    #[test]
    fn wrapped_unicode_pointer_selection_preserves_source_range_on_resize() {
        let mut policy = TextInputPolicy::chat_composer();
        policy.viewport.auto_scroll_to_cursor = false;
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("ab界cd"));
        state.set_content_area(Rect::new(0, 0, 4, 2), &policy);
        control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 2, 0),
        );
        assert_eq!(state.buffer().cursor_byte_index(), 2);
        control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 1, 1),
        );
        assert_eq!(state.buffer().selected_text().as_deref(), Some("界c"));
        assert_eq!(state.buffer().cursor_byte_index(), 6);
        assert_eq!(state.vertical_scroll(), 0);
        state.set_content_area(Rect::new(0, 0, 6, 2), &policy);
        assert_eq!(state.buffer().selected_text().as_deref(), Some("界c"));
        assert_eq!(
            state.wrapped_layout(6).cursor,
            bmux_text_edit::VisualCursor { row: 0, col: 5 }
        );
        let state = RefCell::new(state);
        let component = TextInputComponent::new("editor", &state, &policy).focused(true);
        let layout = component.layout(Constraints::tight(Size::new(6, 2)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.cursor().unwrap().position, Point::new(5, 0));
    }

    #[test]
    fn retained_render_matches_fresh_after_pointer_scroll_and_resize() {
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text(
            "hello 界world\nsecond line\nthird e\u{301} line",
        )));
        for (width, event) in [
            (
                6,
                Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0)),
            ),
            (
                6,
                Event::Mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4, 3)),
            ),
            (9, Event::Key(shift_key(KeyCode::Up))),
            (4, Event::Paste("changed 界".to_string())),
        ] {
            let component = TextInputComponent::new("editor", &state, &policy).focused(true);
            let layout = component.layout(
                Constraints::tight(Size::new(width, 2)),
                &mut LayoutCx::new(),
            );
            let mut event_cx = EventCx::with_clip(&layout, Rect::new(0, 0, width, 2));
            component.event(&event, &layout, &mut event_cx);
            let fresh = RefCell::new(state.borrow().clone());
            *fresh.borrow_mut().wrapped.get_mut() = None;
            let mut retained_buffer = Buffer::empty(Rect::new(0, 0, width, 2));
            let mut fresh_buffer = retained_buffer.clone();
            let mut retained_frame = Frame::new(&mut retained_buffer);
            let mut fresh_frame = Frame::new(&mut fresh_buffer);
            component.paint(&layout, &mut PaintCx::new(&mut retained_frame));
            TextInputComponent::new("editor", &fresh, &policy)
                .focused(true)
                .paint(&layout, &mut PaintCx::new(&mut fresh_frame));
            assert_eq!(retained_frame.buffer(), fresh_frame.buffer());
            assert_eq!(retained_frame.cursor(), fresh_frame.cursor());
            assert_eq!(
                state.borrow().buffer().selection(),
                fresh.borrow().buffer().selection()
            );
            assert_eq!(
                state.borrow().vertical_scroll(),
                fresh.borrow().vertical_scroll()
            );
        }
    }

    #[test]
    fn component_measures_paints_and_handles_events_from_authoritative_layout() {
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text(
            "hello world",
        )));
        let component = TextInputComponent::new("editor", &state, &policy).focused(true);
        let layout = component.layout(Constraints::tight(Size::new(5, 2)), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(5, 2));
        assert_eq!(layout.metadata.semantics, vec!["text-input"]);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 7, 2));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(1, 0, LocalRect::new(0, 0, 5, 2), |cx| {
            component.paint(&layout, cx);
        });
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" hello "));
        assert_eq!(frame.hits().regions()[0].area, Rect::new(1, 0, 5, 2));
        assert_eq!(state.borrow().content_area(), Rect::new(0, 0, 5, 2));

        let mut event_cx = EventCx::with_clip(&layout, Rect::new(0, 0, 5, 2));
        assert_eq!(
            component.event(
                &Event::Key(KeyStroke::simple(KeyCode::Char('!'))),
                &layout,
                &mut event_cx,
            ),
            EventOutcome::Redraw
        );
        assert_eq!(state.borrow().buffer().text(), "hello world!");
    }

    #[test]
    fn outer_selection_is_isolated_by_default_and_requires_explicit_delegation() {
        assert_eq!(
            TextInputOuterSelectionPolicy::default(),
            TextInputOuterSelectionPolicy::Isolated
        );
        assert_eq!(
            TextInputPolicy::raw().outer_selection,
            TextInputOuterSelectionPolicy::Isolated
        );
        assert_eq!(
            TextInputPolicy::chat_composer().outer_selection,
            TextInputOuterSelectionPolicy::Isolated
        );
        assert_eq!(
            TextInputPolicy::raw()
                .outer_selection(TextInputOuterSelectionPolicy::Delegate)
                .outer_selection,
            TextInputOuterSelectionPolicy::Delegate
        );
    }

    #[test]
    fn raw_policy_ignores_keyboard_and_mouse() {
        let policy = TextInputPolicy::raw();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::Left)),
            TextInputOutcome::Ignored
        );
        assert_eq!(
            control.handle_mouse(
                &mut state,
                mouse(MouseEventKind::Down(MouseButton::Left), 1, 0)
            ),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().cursor_byte_index(), "hello".len());
    }

    #[test]
    fn handle_paste_preserves_multiline_text() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        assert_eq!(
            control.handle_paste(&mut state, "\nworld\r\nraw\rtext"),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "hello\nworld\nraw\ntext");
    }

    #[test]
    fn handle_event_dispatches_paste() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::default();

        assert_eq!(
            control.handle_event(&mut state, &Event::Paste("one\ntwo".to_owned())),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "one\ntwo");
    }

    #[test]
    fn shift_selection_extends_buffer_selection() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        assert_eq!(
            control.handle_key(&mut state, shift_key(KeyCode::Left)),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().selected_text(), Some("o".to_string()));
    }

    #[test]
    fn edge_keys_emit_history_outcomes() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::Down)),
            TextInputOutcome::EdgeDown
        );
        state.buffer_mut().move_cursor(TextMotion::Start);
        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::Up)),
            TextInputOutcome::EdgeUp
        );
    }

    #[test]
    fn double_click_selects_word_and_triple_click_selects_all() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello world"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
        );
        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
        );
        assert_eq!(state.buffer().selected_text(), Some("hello".to_string()));

        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
        );
        assert_eq!(
            state.buffer().selected_text(),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn drag_extends_selection() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello world"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        );
        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 5, 0),
        );
        assert_eq!(state.buffer().selected_text(), Some("hello".to_string()));
    }

    #[test]
    fn drag_below_input_scrolls_and_extends_selection() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("0\n1\n2\n3\n4"));
        state.buffer_mut().move_cursor(TextMotion::Start);
        state.set_content_area(Rect::new(0, 0, 10, 2), &policy);

        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        );
        assert_eq!(
            control.handle_mouse(
                &mut state,
                mouse(MouseEventKind::Drag(MouseButton::Left), 0, 2),
            ),
            TextInputOutcome::Redraw
        );

        assert_eq!(state.vertical_scroll(), 1);
        assert_eq!(state.buffer().selected_text(), Some("0\n1\n".to_string()));
    }

    #[test]
    fn drag_above_input_scrolls_and_extends_selection() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("0\n1\n2\n3\n4"));
        state.set_content_area(Rect::new(0, 5, 10, 2), &policy);
        assert_eq!(state.vertical_scroll(), 3);

        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 6),
        );
        assert_eq!(
            control.handle_mouse(
                &mut state,
                mouse(MouseEventKind::Drag(MouseButton::Left), 0, 4),
            ),
            TextInputOutcome::Redraw
        );

        assert_eq!(state.vertical_scroll(), 2);
        assert_eq!(state.buffer().selected_text(), Some("2\n3\n".to_string()));
    }
}
