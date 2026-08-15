//! Higher-level text-input control with opt-in behavior policies.

use std::time::{Duration, Instant};

use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::keyboard::TextKeymap;
use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use unicode_segmentation::UnicodeSegmentation;

const DEFAULT_MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);
const DEFAULT_MULTI_CLICK_DISTANCE: u16 = 2;

/// Stateful text input data used by [`TextInputControl`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputState {
    buffer: TextEditBuffer,
    content_area: Rect,
    vertical_scroll: usize,
    mouse_selection: MouseSelectionState,
}

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
            vertical_scroll: 0,
            mouse_selection: MouseSelectionState::default(),
        }
    }

    /// Return the edit buffer.
    #[must_use]
    pub const fn buffer(&self) -> &TextEditBuffer {
        &self.buffer
    }

    /// Return the mutable edit buffer.
    pub const fn buffer_mut(&mut self) -> &mut TextEditBuffer {
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
        self.vertical_scroll
    }

    /// Store the latest content area.
    pub fn set_content_area(&mut self, area: Rect, policy: &TextInputPolicy) {
        self.content_area = area;
        self.sync_scroll_to_cursor(policy);
    }

    /// Synchronize vertical scroll so the cursor is visible if policy allows.
    pub fn sync_scroll_to_cursor(&mut self, policy: &TextInputPolicy) {
        let Some(offset) = self.cursor_scroll_offset(policy) else {
            return;
        };
        self.vertical_scroll = offset;
    }

    /// Return the scroll offset that keeps the cursor visible.
    #[must_use]
    pub fn cursor_scroll_offset(&self, policy: &TextInputPolicy) -> Option<usize> {
        if !policy.viewport.auto_scroll_to_cursor || self.content_area.height == 0 {
            return None;
        }
        let layout = self
            .buffer
            .wrapped_layout(usize::from(self.content_area.width.max(1)));
        Some(scroll_offset_for_cursor_row(
            layout.cursor.row,
            self.content_area.height,
        ))
    }

    /// Return whether a mouse selection drag is active.
    #[must_use]
    pub const fn mouse_selection_active(&self) -> bool {
        !matches!(self.mouse_selection.active, SelectionGranularity::Disabled)
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
        let wrapped_rows = state
            .buffer
            .wrapped_layout(usize::from(width.max(1)))
            .lines
            .len()
            .max(1);
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
        state.buffer.paste(text);
        if self.policy.viewport.auto_scroll_to_cursor {
            state.vertical_scroll = usize::MAX;
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
            extend_selection(&mut state.buffer, state.content_area, motion);
            state.sync_scroll_to_cursor(self.policy);
            return TextInputOutcome::Edited;
        }
        if let Some(outcome) = self.handle_edge_key(state, stroke) {
            return outcome;
        }
        let Some(command) = self.policy.keyboard.keymap.command_for_key(stroke) else {
            return TextInputOutcome::Ignored;
        };
        state.buffer.apply_command(command);
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
        let width = usize::from(state.content_area.width.max(1));
        let layout = state.buffer.wrapped_layout(width);
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
        apply_selection_granularity(&mut state.buffer, state.content_area, row, col, granularity);
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
        extend_selection_to_granularity(
            &mut state.buffer,
            state.content_area,
            position.row,
            position.col,
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
            state.buffer.insert_newline();
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

fn extend_selection(buffer: &mut TextEditBuffer, area: Rect, motion: TextMotion) {
    match motion {
        TextMotion::VisualUp => extend_visual_selection(buffer, area, -1),
        TextMotion::VisualDown => extend_visual_selection(buffer, area, 1),
        motion => buffer.move_cursor_with_selection(motion, SelectionMode::Extend),
    }
}

fn extend_visual_selection(buffer: &mut TextEditBuffer, area: Rect, delta: isize) {
    let width = usize::from(area.width.max(1));
    let layout = buffer.wrapped_layout(width);
    let target_row = if delta.is_negative() {
        layout.cursor.row.saturating_sub(delta.unsigned_abs())
    } else {
        layout
            .cursor
            .row
            .saturating_add(delta.unsigned_abs())
            .min(layout.lines.len().saturating_sub(1))
    };
    buffer.select_to_wrapped_position(width, target_row, layout.cursor.col);
}

fn scroll_offset_for_cursor_row(cursor_row: usize, height: u16) -> usize {
    cursor_row
        .saturating_add(1)
        .saturating_sub(usize::from(height))
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
        usize::from(mouse.position.y.saturating_sub(area.y)).saturating_add(state.vertical_scroll),
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
        let previous = state.vertical_scroll;
        state.vertical_scroll = state.vertical_scroll.saturating_sub(1);
        return Some(DragPosition {
            row: state.vertical_scroll,
            col,
            scrolled: state.vertical_scroll != previous,
        });
    }
    if mouse.position.y >= area.bottom() {
        if !edge_scroll {
            return None;
        }
        let previous = state.vertical_scroll;
        state.vertical_scroll = state
            .vertical_scroll
            .saturating_add(1)
            .min(max_vertical_scroll(&state.buffer, area));
        return Some(DragPosition {
            row: state
                .vertical_scroll
                .saturating_add(usize::from(area.height).saturating_sub(1)),
            col,
            scrolled: state.vertical_scroll != previous,
        });
    }
    Some(DragPosition {
        row: usize::from(mouse.position.y.saturating_sub(area.y))
            .saturating_add(state.vertical_scroll),
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

fn max_vertical_scroll(buffer: &TextEditBuffer, area: Rect) -> usize {
    buffer
        .wrapped_layout(usize::from(area.width.max(1)))
        .lines
        .len()
        .saturating_sub(usize::from(area.height))
}

fn apply_selection_granularity(
    buffer: &mut TextEditBuffer,
    area: Rect,
    row: usize,
    col: usize,
    granularity: SelectionGranularity,
) {
    let width = usize::from(area.width.max(1));
    let byte_index = buffer.byte_index_for_wrapped_position(width, row, col);
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
    area: Rect,
    row: usize,
    col: usize,
    granularity: SelectionGranularity,
) {
    let width = usize::from(area.width.max(1));
    let byte_index = buffer.byte_index_for_wrapped_position(width, row, col);
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
    use bmux_tui::geometry::Point;

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
