//! Higher-level text-input control with opt-in behavior policies.

use std::cell::{Cell, RefCell};
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
    logical_width: u64,
    viewport_height: u64,
    pointer_offset: (usize, usize),
    scroll: ScrollViewState,
    mouse_selection: MouseSelectionState,
    wrapped: RefCell<Option<(u64, usize, bmux_text_edit::WrapLayout)>>,
    exclusion_columns: u16,
    exclusion_key: Cell<Option<bmux_text_edit::WrapExclusion>>,
}

// Derived measurement state must not change the meaning of editor equality.
impl PartialEq for TextInputState {
    fn eq(&self, other: &Self) -> bool {
        self.buffer == other.buffer
            && self.content_area == other.content_area
            && self.viewport_height == other.viewport_height
            && self.logical_width == other.logical_width
            && self.pointer_offset == other.pointer_offset
            && self.scroll == other.scroll
            && self.exclusion_columns == other.exclusion_columns
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
    /// Capture ephemeral rebuild state, not a stable wire/storage contract.
    /// Derived wrap geometry is rebuilt; editor undo history is not serialized.
    #[must_use]
    pub fn snapshot(&self) -> serde_json::Value {
        let snapshot = TextInputSnapshot {
            buffer: &self.buffer,
            content_area: (
                self.content_area.x,
                self.content_area.y,
                self.content_area.width,
                self.content_area.height,
            ),
            logical_width: self.logical_width,
            viewport_height: self.viewport_height,
            pointer_offset: self.pointer_offset,
            scroll: self.scroll,
            active: self.mouse_selection.active,
            last_click: self.mouse_selection.last_click.map(|click| ClickSnapshot {
                x: click.x,
                y: click.y,
                age: click.at.elapsed().min(Duration::from_secs(1)),
                count: click.count,
            }),
        };
        crate::common::state_snapshot(&snapshot)
    }

    /// Restore bounded ephemeral native state atomically. Reapply canonical text
    /// afterwards if needed. Cached wrapping and undo history are reconstructed.
    ///
    /// # Errors
    /// Rejects malformed, oversized snapshots or invalid UTF-8/grapheme positions.
    pub fn restore_snapshot(&mut self, snapshot: &serde_json::Value) -> Result<(), String> {
        crate::common::validate_state_snapshot(snapshot)?;
        let buffer = snapshot.get("buffer").ok_or("missing editor buffer")?;
        let text = buffer
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing editor text")?;
        for field in ["cursor", "selection_anchor"] {
            let position = buffer.get(field).ok_or("missing editor position")?;
            if field == "selection_anchor" && position.is_null() {
                continue;
            }
            let index = position
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("invalid editor position")?;
            if index != text.len()
                && !text
                    .grapheme_indices(true)
                    .any(|(offset, _)| offset == index)
            {
                return Err("editor position is not a grapheme boundary".into());
            }
        }
        let restored: TextInputSnapshot = crate::common::decode_state_snapshot(snapshot)?;
        let last_click = restored
            .last_click
            .map(|click| {
                if !(1..=3).contains(&click.count) || click.age > Duration::from_secs(1) {
                    return Err("invalid editor click state".to_owned());
                }
                let at = Instant::now()
                    .checked_sub(click.age)
                    .ok_or("invalid editor click time")?;
                Ok(MouseClickState {
                    x: click.x,
                    y: click.y,
                    at,
                    count: click.count,
                })
            })
            .transpose()?;
        *self = Self {
            buffer: restored.buffer,
            content_area: Rect::new(
                restored.content_area.0,
                restored.content_area.1,
                restored.content_area.2,
                restored.content_area.3,
            ),
            logical_width: restored.logical_width,
            viewport_height: restored.viewport_height,
            pointer_offset: restored.pointer_offset,
            scroll: restored.scroll,
            mouse_selection: MouseSelectionState {
                last_click,
                active: restored.active,
            },
            wrapped: RefCell::new(None),
            exclusion_columns: 0,
            exclusion_key: Cell::new(None),
        };
        Ok(())
    }

    /// Create state around an edit buffer.
    #[must_use]
    pub fn new(buffer: TextEditBuffer) -> Self {
        Self {
            buffer,
            content_area: Rect::new(0, 0, 1, 1),
            logical_width: 1,
            viewport_height: 1,
            pointer_offset: (0, 0),
            scroll: ScrollViewState::new(),
            mouse_selection: MouseSelectionState::default(),
            wrapped: RefCell::new(None),
            exclusion_columns: 0,
            exclusion_key: Cell::new(None),
        }
    }

    /// Reserve right-edge cells on the last visible editor row. All other rows
    /// retain full width; cursor, selection and mouse routing share this layout.
    pub fn set_trailing_exclusion(&mut self, columns: u16, policy: &TextInputPolicy) {
        self.exclusion_columns = columns;
        self.sync_scroll_to_cursor(policy);
    }

    /// Measure the minimum content height beside a bottom-right region.
    ///
    /// At most two projections are needed: full-width wrapping, then wrapping
    /// with the final row shortened. The result is clamped to `maximum`.
    #[must_use]
    pub fn rows_with_trailing_exclusion(&self, width: u16, columns: u16, maximum: u16) -> u16 {
        let maximum = maximum.max(1);
        let base = self
            .buffer
            .wrapped_layout(usize::from(width))
            .lines
            .len()
            .max(1);
        let rows = base.min(usize::from(maximum));
        if base > usize::from(maximum) || columns == 0 {
            return u16::try_from(rows).unwrap_or(maximum);
        }
        let layout = self.buffer.wrapped_layout_with_exclusion(
            usize::from(width),
            Some(bmux_text_edit::WrapExclusion {
                row: rows - 1,
                columns: usize::from(columns),
            }),
        );
        u16::try_from(layout.lines.len().max(rows))
            .unwrap_or(maximum)
            .min(maximum)
    }

    fn wrapped_layout(&self, width: u64) -> std::cell::Ref<'_, bmux_text_edit::WrapLayout> {
        let exclusion = (self.exclusion_columns > 0 && self.viewport_height > 0).then(|| {
            bmux_text_edit::WrapExclusion {
                row: usize::try_from(
                    self.scroll
                        .vertical_offset()
                        .saturating_add(self.viewport_height)
                        .saturating_sub(1),
                )
                .unwrap_or(usize::MAX),
                columns: usize::from(self.exclusion_columns),
            }
        });
        if self.exclusion_key.replace(exclusion) != exclusion {
            *self.wrapped.borrow_mut() = None;
        }
        let stale = self
            .wrapped
            .borrow()
            .as_ref()
            .is_none_or(|(cached_width, _, _)| *cached_width != width);
        if stale {
            *self.wrapped.borrow_mut() = Some((
                width,
                self.buffer.cursor_byte_index(),
                self.buffer.wrapped_layout_with_exclusion(
                    usize::try_from(width.max(1)).unwrap_or(usize::MAX),
                    exclusion,
                ),
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
    pub const fn vertical_scroll(&self) -> u64 {
        self.scroll.vertical_offset()
    }

    /// Store the latest content area.
    pub fn set_content_area(&mut self, area: Rect, policy: &TextInputPolicy) {
        self.viewport_height = u64::from(area.height);
        self.set_geometry(area, u64::from(area.width), (0, 0), policy);
    }

    fn set_geometry(
        &mut self,
        area: Rect,
        width: u64,
        offset: (usize, usize),
        policy: &TextInputPolicy,
    ) {
        if self.logical_width.max(1) != width.max(1) {
            self.buffer.reset_preferred_visual_column();
        }
        self.content_area = area;
        self.logical_width = width;
        self.pointer_offset = offset;
        self.sync_scroll_to_cursor(policy);
        let rows = self.wrapped_layout(self.logical_width).lines.len();
        let viewport = editor_viewport(self.scroll_area(), rows);
        ScrollView::scroll_vertical_by(&viewport, &mut self.scroll, 0);
    }

    const fn scroll_area(&self) -> LogicalSize {
        LogicalSize::new(self.logical_width, self.viewport_height)
    }

    /// Synchronize vertical scroll so the cursor is visible if policy allows.
    pub fn sync_scroll_to_cursor(&mut self, policy: &TextInputPolicy) {
        let Some(offset) = self.cursor_scroll_offset(policy) else {
            return;
        };
        self.scroll.set_vertical_offset(offset);
        if self.exclusion_columns > 0
            && let Some(offset) = self.cursor_scroll_offset(policy)
        {
            self.scroll.set_vertical_offset(offset);
        }
    }

    /// Return the scroll offset that keeps the cursor visible.
    #[must_use]
    pub fn cursor_scroll_offset(&self, policy: &TextInputPolicy) -> Option<u64> {
        if !policy.viewport.auto_scroll_to_cursor || self.viewport_height == 0 {
            return None;
        }
        let layout = self.wrapped_layout(self.logical_width);
        let viewport = editor_viewport(
            self.scroll_area(),
            layout.lines.len().max(layout.cursor.row.saturating_add(1)),
        );
        // Preserve the editor's cursor-at-bottom policy while sharing reveal/clamping.
        let mut scroll = if self.exclusion_columns == 0 {
            ScrollViewState::new()
        } else {
            self.scroll
        };
        ScrollView::new().ensure_visible(
            &viewport,
            &mut scroll,
            u64::try_from(layout.cursor.row).unwrap_or(u64::MAX),
            1,
        );
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
    pointer_intent: bmux_tui::pointer::PointerIntent,
    id: LayoutId,
    state: &'state RefCell<TextInputState>,
    policy: &'policy TextInputPolicy,
    history_bindings: TextInputHistoryBindings<'policy>,
    style: Style,
    selection_style: Style,
    placeholder: Option<&'policy str>,
    placeholder_style: Style,
    focused: bool,
    disabled: bool,
    trailing_exclusion: u16,
}

impl<'policy, 'state> TextInputComponent<'policy, 'state> {
    /// Reserve cells at the right edge of the bottom visible row. The caller
    /// places its floating child in this region using the same measured width.
    #[must_use]
    pub const fn trailing_exclusion(mut self, columns: u16) -> Self {
        self.trailing_exclusion = columns;
        self
    }

    /// Override pointer presentation without changing input routing or mouse policy.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: bmux_tui::pointer::PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Create an editable text component with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        state: &'state RefCell<TextInputState>,
        policy: &'policy TextInputPolicy,
    ) -> Self {
        Self {
            id: id.into(),
            pointer_intent: bmux_tui::pointer::PointerIntent::Auto,
            state,
            policy,
            history_bindings: TextInputHistoryBindings::standard(),
            style: Style::new(),
            selection_style: Style::new().add_modifier(bmux_tui::style::Modifier::REVERSED),
            placeholder: None,
            placeholder_style: Style::new(),
            focused: false,
            disabled: false,
            trailing_exclusion: 0,
        }
    }

    /// Replace history defaults with authoritative remapped or disabled bindings.
    #[must_use]
    pub const fn with_history_bindings(
        mut self,
        bindings: TextInputHistoryBindings<'policy>,
    ) -> Self {
        self.history_bindings = bindings;
        self
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

        self.trailing_exclusion.hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
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
        let rows = if self.trailing_exclusion == 0 {
            TextInputControl::new(self.policy)
                .visible_rows_for_width(&state, width.try_into().unwrap_or(u16::MAX))
        } else {
            let maximum = constraints
                .max_height()
                .unwrap_or_else(|| u64::from(u16::MAX))
                .min(u64::from(self.policy.viewport.max_rows.unwrap_or(u16::MAX)));
            state
                .rows_with_trailing_exclusion(
                    width.try_into().unwrap_or(u16::MAX),
                    self.trailing_exclusion,
                    maximum.try_into().unwrap_or(u16::MAX),
                )
                .max(self.policy.viewport.min_rows.max(1))
        };
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                width,
                u64::try_from(usize::from(rows)).unwrap_or(u64::MAX),
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("text-input"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let local = LocalRect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            height,
        );
        let area = Rect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            height,
        );
        let mut state = self.state.borrow_mut();
        state.viewport_height = layout.size.height;
        state.exclusion_columns = self.trailing_exclusion;
        state.set_geometry(area, layout.size.width, (0, 0), self.policy);
        let wrapped = state.wrapped_layout(state.logical_width);
        let mut input = TextInput::new(state.buffer())
            .wrapped_layout(&wrapped)
            .trailing_exclusion(self.trailing_exclusion)
            .id(self.id.clone())
            .style(self.style)
            .selection_style(self.selection_style)
            .placeholder_style(self.placeholder_style)
            .cursor_visible(self.focused && !self.disabled)
            .vertical_scroll(usize::try_from(state.vertical_scroll()).unwrap_or(usize::MAX));
        if let Some(placeholder) = self.placeholder {
            input = input.placeholder(placeholder);
        }
        input.paint(layout, cx);
        let regions = if self.trailing_exclusion > 0 {
            vec![
                Rect::new(0, 0, area.width, height.saturating_sub(1)),
                Rect::new(
                    0,
                    height.saturating_sub(1),
                    area.width.saturating_sub(self.trailing_exclusion),
                    height.min(1),
                ),
            ]
        } else {
            vec![area]
        };
        let clip = cx.area();
        cx.with_child(
            clip.x,
            clip.y,
            LocalRect::new(0, 0, clip.width, clip.height),
            |cx| {
                let visible = Rect::new(0, 0, clip.width, clip.height);
                let visible_regions = if self.trailing_exclusion == 0 {
                    vec![visible]
                } else {
                    regions
                        .into_iter()
                        .filter_map(|region| {
                            let left = i64::from(region.x).max(i64::from(clip.x));
                            let top = i64::from(region.y).max(clip.y);
                            let right = i64::from(region.right())
                                .min(i64::from(clip.x) + i64::from(clip.width));
                            let bottom =
                                i64::from(region.bottom()).min(clip.y + i64::from(clip.height));
                            (right > left && bottom > top).then(|| {
                                Rect::new(
                                    u16::try_from(left - i64::from(clip.x)).unwrap_or(u16::MAX),
                                    u16::try_from(top - clip.y).unwrap_or(u16::MAX),
                                    u16::try_from(right - left).unwrap_or(u16::MAX),
                                    u16::try_from(bottom - top).unwrap_or(u16::MAX),
                                )
                            })
                        })
                        .collect()
                };
                for region in visible_regions {
                    cx.push_hit(
                        HitRegion::new(self.id.as_str(), region)
                            .role(HitRole::TextInput)
                            .pointer_intent(self.pointer_intent)
                            .hoverable(true)
                            .focusable(true)
                            .enabled(!self.disabled),
                    );
                }
                cx.push_semantic(SemanticRegion::new(
                    self.id.as_str(),
                    Rect::new(0, 0, clip.width, clip.height),
                    "text-input",
                ));
            },
        );
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
        state.viewport_height = layout.size.height;
        let (x, y) = cx.local_point(bmux_tui::geometry::Point::new(area.x, area.y));
        state.exclusion_columns = self.trailing_exclusion;
        state.set_geometry(
            area,
            layout.size.width,
            (
                usize::try_from(x.max(0)).unwrap_or(usize::MAX),
                usize::try_from(y.max(0)).unwrap_or(usize::MAX),
            ),
            self.policy,
        );
        match TextInputControl::new(self.policy)
            .with_history_bindings(self.history_bindings)
            .handle_event(&mut state, event)
        {
            TextInputOutcome::Ignored => EventOutcome::Ignored,
            TextInputOutcome::Edited
            | TextInputOutcome::Redraw
            | TextInputOutcome::Submitted
            | TextInputOutcome::EdgeUp
            | TextInputOutcome::EdgeDown => EventOutcome::Redraw,
        }
    }
}

/// Semantic editor history operation resolved by a caller-owned keymap adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputHistoryAction {
    /// Restore the preceding edit state.
    Undo,
    /// Restore the most recently undone edit state.
    Redo,
}

const DEFAULT_UNDO_KEYS: &[KeyStroke] = &[
    KeyStroke::with_modifiers(
        KeyCode::Char('z'),
        Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    ),
    KeyStroke::with_modifiers(
        KeyCode::Char('z'),
        Modifiers {
            super_key: true,
            ..Modifiers::NONE
        },
    ),
];
const DEFAULT_REDO_KEYS: &[KeyStroke] = &[
    KeyStroke::with_modifiers(
        KeyCode::Char('z'),
        Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::NONE
        },
    ),
    KeyStroke::with_modifiers(
        KeyCode::Char('z'),
        Modifiers {
            super_key: true,
            shift: true,
            ..Modifiers::NONE
        },
    ),
    KeyStroke::with_modifiers(
        KeyCode::Char('y'),
        Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    ),
];

/// Authoritative history bindings supplied by the application's generic keymap.
///
/// Empty lists independently disable each action. No default fallback is applied
/// to explicitly supplied lists. A stroke present in both lists fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInputHistoryBindings<'keys> {
    /// Undo strokes, or an empty slice to disable undo.
    pub undo: &'keys [KeyStroke],
    /// Redo strokes, or an empty slice to disable redo.
    pub redo: &'keys [KeyStroke],
}

impl Default for TextInputHistoryBindings<'_> {
    fn default() -> Self {
        Self::standard()
    }
}

impl TextInputHistoryBindings<'_> {
    /// Conventional ctrl/super-Z undo, ctrl/super-shift-Z and ctrl-Y redo.
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            undo: DEFAULT_UNDO_KEYS,
            redo: DEFAULT_REDO_KEYS,
        }
    }

    /// Disable both actions without disabling ordinary keyboard editing.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            undo: &[],
            redo: &[],
        }
    }

    /// Resolve exactly one action; conflicting or absent bindings return `None`.
    #[must_use]
    pub fn action_for_key(self, stroke: KeyStroke) -> Option<TextInputHistoryAction> {
        match (self.undo.contains(&stroke), self.redo.contains(&stroke)) {
            (true, false) => Some(TextInputHistoryAction::Undo),
            (false, true) => Some(TextInputHistoryAction::Redo),
            _ => None,
        }
    }
}

/// Stateless text-input event controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInputControl<'policy> {
    policy: &'policy TextInputPolicy,
    history_bindings: TextInputHistoryBindings<'policy>,
}

impl<'policy> TextInputControl<'policy> {
    /// Create a control using `policy`.
    #[must_use]
    pub const fn new(policy: &'policy TextInputPolicy) -> Self {
        Self {
            policy,
            history_bindings: TextInputHistoryBindings::standard(),
        }
    }

    /// Replace history defaults with authoritative remapped or disabled bindings.
    #[must_use]
    pub const fn with_history_bindings(
        mut self,
        bindings: TextInputHistoryBindings<'policy>,
    ) -> Self {
        self.history_bindings = bindings;
        self
    }

    /// Return the configured policy.
    #[must_use]
    pub const fn policy(&self) -> &TextInputPolicy {
        self.policy
    }

    /// Return visible content rows for a terminal width.
    #[must_use]
    pub fn visible_rows_for_width(&self, state: &TextInputState, width: u16) -> u16 {
        let wrapped_rows = state.wrapped_layout(u64::from(width)).lines.len().max(1);
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
        state.sync_scroll_to_cursor(self.policy);
        TextInputOutcome::Edited
    }

    /// Handle one keyboard stroke.
    pub fn handle_key(&self, state: &mut TextInputState, stroke: KeyStroke) -> TextInputOutcome {
        self.handle_key_with_history_action(
            state,
            stroke,
            self.history_bindings.action_for_key(stroke),
        )
    }

    /// Handle a stroke with the authoritative history result from a semantic adapter.
    ///
    /// `None` suppresses all default history bindings, while ordinary editing still
    /// uses the configured text keymap. Disabled keyboard policy overrides actions.
    pub fn handle_key_with_history_action(
        &self,
        state: &mut TextInputState,
        stroke: KeyStroke,
        history_action: Option<TextInputHistoryAction>,
    ) -> TextInputOutcome {
        if !self.policy.keyboard.enabled {
            return TextInputOutcome::Ignored;
        }
        if let Some(action) = history_action {
            let changed = match action {
                TextInputHistoryAction::Undo => state.buffer_mut().undo(),
                TextInputHistoryAction::Redo => state.buffer_mut().redo(),
            };
            if changed {
                state.sync_scroll_to_cursor(self.policy);
                return TextInputOutcome::Edited;
            }
            // Recognized empty-stack commands must not become inserted text or
            // leak into unrelated application shortcuts.
            return TextInputOutcome::Redraw;
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
        match command {
            bmux_text_edit::TextEditCommand::Move(TextMotion::VisualUp) => {
                move_visual_cursor(state, -1, SelectionMode::Move);
            }
            bmux_text_edit::TextEditCommand::Move(TextMotion::VisualDown) => {
                move_visual_cursor(state, 1, SelectionMode::Move);
            }
            bmux_text_edit::TextEditCommand::Move(_) => {
                state.buffer.apply_command(command);
            }
            _ => {
                state.buffer_mut().apply_command(command);
            }
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
        let layout = state.wrapped_layout(state.logical_width);
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
            .wrapped_layout(state.logical_width)
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
            .wrapped_layout(state.logical_width)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    move_visual_cursor(state, delta, SelectionMode::Extend);
}

fn move_visual_cursor(state: &mut TextInputState, delta: isize, selection: SelectionMode) {
    drop(state.wrapped_layout(state.logical_width));
    let cached = state.wrapped.borrow();
    let layout = &cached.as_ref().expect("measured projection").2;
    state.buffer.move_cursor_in_layout(layout, delta, selection);
}

fn pointer_source_row(state: &TextInputState, visible_row: usize) -> usize {
    visible_row
        .saturating_add(state.pointer_offset.1)
        .saturating_add(usize::try_from(state.vertical_scroll()).unwrap_or(usize::MAX))
}

fn mouse_wrapped_position(state: &TextInputState, mouse: MouseEvent) -> Option<(usize, usize)> {
    let area = state.content_area;
    if mouse.position.y < area.y || mouse.position.y >= area.bottom() {
        return None;
    }
    if mouse.position.x < area.x || mouse.position.x >= area.right() {
        return None;
    }
    if state.exclusion_columns > 0
        && mouse.position.y == area.bottom().saturating_sub(1)
        && mouse.position.x >= area.right().saturating_sub(state.exclusion_columns)
    {
        return None;
    }
    Some((
        pointer_source_row(state, usize::from(mouse.position.y.saturating_sub(area.y))),
        usize::from(mouse.position.x.saturating_sub(area.x)).saturating_add(state.pointer_offset.0),
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
    let col = clamped_mouse_col(area, mouse.position.x).saturating_add(state.pointer_offset.0);
    if mouse.position.y < area.y {
        if !edge_scroll {
            return None;
        }
        let previous = state.vertical_scroll();
        let viewport = editor_viewport(
            state.scroll_area(),
            state.wrapped_layout(state.logical_width).lines.len(),
        );
        ScrollView::scroll_vertical_by(&viewport, &mut state.scroll, -1);
        return Some(DragPosition {
            row: pointer_source_row(state, 0),
            col,
            scrolled: state.vertical_scroll() != previous,
        });
    }
    if mouse.position.y >= area.bottom() {
        if !edge_scroll {
            return None;
        }
        let previous = state.vertical_scroll();
        let viewport = editor_viewport(
            state.scroll_area(),
            state.wrapped_layout(state.logical_width).lines.len(),
        );
        ScrollView::scroll_vertical_by(&viewport, &mut state.scroll, 1);
        return Some(DragPosition {
            row: pointer_source_row(state, usize::from(area.height).saturating_sub(1)),
            col,
            scrolled: state.vertical_scroll() != previous,
        });
    }
    Some(DragPosition {
        row: pointer_source_row(state, usize::from(mouse.position.y.saturating_sub(area.y))),
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

fn editor_viewport(area: LogicalSize, rows: usize) -> LayoutNode {
    ScrollViewComponent::viewport_layout(
        LayoutId::new("text-input.viewport"),
        area,
        LayoutNode::leaf(
            LayoutId::new("text-input.content"),
            LogicalSize::new(area.width, rows.try_into().unwrap_or(u64::MAX)),
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

    #[test]
    fn pointer_override_invalidates_paint_without_remeasuring_layout() {
        use bmux_tui::pointer::{PointerIntent, PointerShape};

        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text("a")));
        state.borrow_mut().buffer_mut().insert_str("b");
        let component = TextInputComponent::new("editor", &state, &policy)
            .with_history_bindings(TextInputHistoryBindings::disabled())
            .focused(true);
        let original = component.revision();
        let component = component.pointer_intent(PointerIntent::Hidden);
        assert_eq!(component.pointer_intent, PointerIntent::Hidden);
        let hidden = component.revision();
        assert_eq!(original.layout, hidden.layout);
        assert_ne!(original.paint, hidden.paint);
        let component = component.pointer_intent(PointerIntent::Shape(PointerShape::Crosshair));
        assert_eq!(
            component.pointer_intent,
            PointerIntent::Shape(PointerShape::Crosshair)
        );
        assert_ne!(hidden.paint, component.revision().paint);
    }

    #[test]
    fn undo_redo_controls_restore_atomic_edits_and_consume_empty_history() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("é token"));
        let before = state.buffer().edit_state();
        state
            .buffer_mut()
            .replace_byte_range(&before, 3..8, "🦀", 7, None)
            .unwrap();
        let after = state.buffer().edit_state();
        let undo = KeyStroke::with_modifiers(
            KeyCode::Char('z'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        let redo = KeyStroke::with_modifiers(
            KeyCode::Char('z'),
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(
            control.handle_key(&mut state, undo),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().edit_state(), before);
        assert_eq!(
            control.handle_key(&mut state, undo),
            TextInputOutcome::Redraw
        );
        assert_eq!(state.buffer().edit_state(), before);
        assert_eq!(
            control.handle_key(&mut state, redo),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().edit_state(), after);
        let super_undo = KeyStroke::with_modifiers(
            KeyCode::Char('z'),
            Modifiers {
                super_key: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(
            control.handle_key(&mut state, super_undo),
            TextInputOutcome::Edited
        );
        let ctrl_y = KeyStroke::with_modifiers(
            KeyCode::Char('y'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(
            control.handle_key(&mut state, ctrl_y),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().edit_state(), after);
        assert!(
            TextInputHistoryBindings::standard()
                .action_for_key(KeyStroke::with_modifiers(
                    KeyCode::Char('z'),
                    Modifiers {
                        ctrl: true,
                        alt: true,
                        ..Modifiers::NONE
                    }
                ))
                .is_none()
        );
        let mut disabled = policy;
        disabled.keyboard.enabled = false;
        assert_eq!(
            TextInputControl::new(&disabled).handle_key(&mut state, undo),
            TextInputOutcome::Ignored
        );
    }

    #[test]
    fn history_bindings_remap_disable_and_semantic_adapter_are_authoritative() {
        let policy = TextInputPolicy::chat_composer();
        let undo_keys = [key(KeyCode::F(7))];
        let redo_keys = [key(KeyCode::F(8))];
        let bindings = TextInputHistoryBindings {
            undo: &undo_keys,
            redo: &redo_keys,
        };
        let control = TextInputControl::new(&policy).with_history_bindings(bindings);
        let mut state = TextInputState::new(TextEditBuffer::from_text("a"));
        state.buffer_mut().insert_str("b");
        let default_undo = KeyStroke::with_modifiers(
            KeyCode::Char('z'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        );
        assert_eq!(
            control.handle_key(&mut state, default_undo),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().text(), "ab");
        assert_eq!(
            control.handle_key(&mut state, undo_keys[0]),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "a");
        assert_eq!(
            control.handle_key(&mut state, undo_keys[0]),
            TextInputOutcome::Redraw
        );
        let undo_disabled =
            TextInputControl::new(&policy).with_history_bindings(TextInputHistoryBindings {
                undo: &[],
                redo: &redo_keys,
            });
        assert_eq!(
            undo_disabled.handle_key(&mut state, default_undo),
            TextInputOutcome::Ignored
        );
        assert_eq!(
            undo_disabled.handle_key(&mut state, redo_keys[0]),
            TextInputOutcome::Edited
        );
        let redo_disabled =
            TextInputControl::new(&policy).with_history_bindings(TextInputHistoryBindings {
                undo: &undo_keys,
                redo: &[],
            });
        redo_disabled.handle_key(&mut state, undo_keys[0]);
        assert_eq!(
            redo_disabled.handle_key(&mut state, redo_keys[0]),
            TextInputOutcome::Ignored
        );
        assert!(state.buffer().can_redo());
        let disabled = TextInputControl::new(&policy)
            .with_history_bindings(TextInputHistoryBindings::disabled());
        assert_eq!(
            disabled.handle_key(&mut state, default_undo),
            TextInputOutcome::Ignored
        );
        assert_eq!(
            disabled.handle_key_with_history_action(
                &mut state,
                redo_keys[0],
                Some(TextInputHistoryAction::Redo)
            ),
            TextInputOutcome::Edited
        );
        assert_eq!(
            TextInputControl::new(&policy).handle_key_with_history_action(
                &mut state,
                default_undo,
                None
            ),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().text(), "ab");
        let conflicts = TextInputHistoryBindings {
            undo: &undo_keys,
            redo: &undo_keys,
        };
        assert!(conflicts.action_for_key(undo_keys[0]).is_none());
        let mut keyboard_disabled = policy;
        keyboard_disabled.keyboard.enabled = false;
        assert_eq!(
            TextInputControl::new(&keyboard_disabled).handle_key_with_history_action(
                &mut state,
                undo_keys[0],
                Some(TextInputHistoryAction::Undo)
            ),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().text(), "ab");
    }

    #[test]
    fn component_uses_authoritative_disabled_history_bindings() {
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text("a")));
        state.borrow_mut().buffer_mut().insert_str("b");
        let component = TextInputComponent::new("editor", &state, &policy)
            .with_history_bindings(TextInputHistoryBindings::disabled())
            .focused(true);
        let layout = LayoutNode::leaf("editor".into(), LogicalSize::new(8, 1));
        let event = Event::Key(KeyStroke::with_modifiers(
            KeyCode::Char('z'),
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        ));
        assert_eq!(
            component.event(&event, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Ignored
        );
        assert_eq!(state.borrow().buffer().text(), "ab");
        assert!(state.borrow().buffer().can_undo());
    }

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
    fn deep_editor_paint_and_click_share_logical_height() {
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text(format!(
            "{}end",
            "x\n".repeat(70_000)
        ))));
        let component = TextInputComponent::new("deep", &state, &policy).focused(true);
        let layout = LayoutNode::leaf("deep".into(), LogicalSize::new(8, 70_001));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame)
            .with_child_size(0, -70_000, layout.size, |cx| component.paint(&layout, cx));
        assert_eq!(frame.cursor().unwrap().position, Point::new(3, 0));
        assert_eq!(state.borrow().vertical_scroll(), 0);
        let row: String = buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert_eq!(row.trim_end(), "end");
        EventCx::new(&layout).with_transform(0, 0, 0, -70_000, Rect::new(0, 0, 8, 1), |cx| {
            component.event(
                &Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0)),
                &layout,
                cx,
            );
        });
        assert_eq!(state.borrow().buffer().cursor_byte_index(), 140_001);
    }

    #[test]
    fn outer_clip_does_not_change_auto_reveal_between_paint_and_input() {
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text(
            "zero\none\ntwo\nthree\nfour",
        )));
        let component = TextInputComponent::new("editor", &state, &policy);
        let layout = LayoutNode::leaf("editor".into(), LogicalSize::new(8, 5));
        let mut buffer = Buffer::empty(Rect::new(3, 4, 8, 2));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame)
            .with_child_size(3, 2, layout.size, |cx| component.paint(&layout, cx));
        assert_eq!(state.borrow().vertical_scroll(), 0);
        let event = Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 4));
        EventCx::new(&layout).with_transform(0, 0, 3, 2, Rect::new(3, 4, 8, 2), |cx| {
            component.event(&event, &layout, cx);
        });
        assert_eq!(state.borrow().vertical_scroll(), 0);
        assert_eq!(state.borrow().buffer().cursor_byte_index(), 9);
        let row: String = buffer.cells()[..8]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert_eq!(row.trim_end(), "two");
    }

    #[test]
    fn vertically_transformed_component_click_targets_visible_source_row() {
        let mut policy = TextInputPolicy::chat_composer();
        policy.viewport.auto_scroll_to_cursor = false;
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text(
            "zero\none\ntwo\nthree\nfour",
        )));
        let component = TextInputComponent::new("editor", &state, &policy);
        let layout = LayoutNode::leaf("editor".into(), LogicalSize::new(8, 5));
        let event = Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 4));
        EventCx::new(&layout).with_transform(0, 0, 3, 2, Rect::new(3, 4, 8, 2), |cx| {
            assert_eq!(component.event(&event, &layout, cx), EventOutcome::Redraw);
        });
        assert_eq!(state.borrow().buffer().cursor_byte_index(), 9);
    }

    #[test]
    fn outer_vertical_offset_is_shared_by_click_and_drag_rows() {
        let mut policy = TextInputPolicy::chat_composer();
        policy.viewport.auto_scroll_to_cursor = false;
        let mut state = TextInputState::new(TextEditBuffer::from_text("0\n1\n2\n3\n4\n5\n6\n7\n8"));
        state.set_geometry(Rect::new(3, 4, 4, 2), 4, (0, 3), &policy);
        state.scroll.set_vertical_offset(1);
        let point = mouse(MouseEventKind::Down(MouseButton::Left), 3, 4);
        assert_eq!(mouse_wrapped_position(&state, point), Some((4, 0)));
        let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 3, 5);
        assert_eq!(
            drag_wrapped_position(&mut state, drag, true).unwrap().row,
            5
        );
        let below = mouse(MouseEventKind::Drag(MouseButton::Left), 3, 6);
        let position = drag_wrapped_position(&mut state, below, true).unwrap();
        assert!(position.scrolled);
        assert_eq!(position.row, 6);
        let above = mouse(MouseEventKind::Drag(MouseButton::Left), 3, 3);
        assert_eq!(
            drag_wrapped_position(&mut state, above, true).unwrap().row,
            4
        );
    }

    #[test]
    fn paste_reveals_cursor_instead_of_document_bottom() {
        let policy = TextInputPolicy::chat_composer();
        let mut state =
            TextInputState::new(TextEditBuffer::from_text("zero\none\ntwo\nthree\nfour"));
        state.buffer_mut().move_cursor_to_wrapped_position(20, 1, 0);
        state.set_content_area(Rect::new(0, 0, 20, 2), &policy);
        TextInputControl::new(&policy).handle_paste(&mut state, "inserted");
        assert_eq!(state.vertical_scroll(), 0);
        assert!(state.buffer().text().contains("insertedone"));
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
    fn width_reflow_resets_preferred_column_but_translation_does_not() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        for (width, expected) in [(8, 16), (4, 11)] {
            let mut state = TextInputState::new(TextEditBuffer::from_text("ab界de\nx\nab界de"));
            state.set_content_area(Rect::new(0, 0, 8, 2), &policy);
            state.buffer_mut().move_cursor(TextMotion::Absolute(6));
            control.handle_key(&mut state, key(KeyCode::Down));
            assert_eq!(state.buffer().cursor_byte_index(), 9);
            // A moved/resized viewport must not move the source cursor or selection.
            state.set_content_area(Rect::new(3, 5, width, 3), &policy);
            assert_eq!(state.buffer().cursor_byte_index(), 9);
            control.handle_key(&mut state, shift_key(KeyCode::Down));
            assert_eq!(state.buffer().cursor_byte_index(), expected);
            assert_eq!(
                state.buffer().selected_text().as_deref(),
                Some(&state.buffer().text()[9..expected])
            );
            assert!(
                u64::try_from(state.wrapped_layout(u64::from(width)).cursor.row).unwrap()
                    < state.vertical_scroll() + 3
            );
        }
    }

    #[test]
    fn keyboard_vertical_motion_preserves_column_across_short_rows() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("abcde\nx\nabcde"));
        state.set_content_area(Rect::new(0, 0, 5, 2), &policy);
        state.buffer_mut().move_cursor(TextMotion::Absolute(4));
        control.handle_key(&mut state, key(KeyCode::Down));
        assert_eq!(state.buffer().cursor_byte_index(), 7);
        control.handle_key(&mut state, shift_key(KeyCode::Down));
        assert_eq!(state.buffer().cursor_byte_index(), 12);
        assert_eq!(state.vertical_scroll(), 1);
        assert_eq!(state.buffer().selected_text().as_deref(), Some("\nabcd"));
        control.handle_key(&mut state, key(KeyCode::Up));
        assert!(state.buffer().selection().is_none());
        control.handle_key(&mut state, key(KeyCode::Up));
        assert_eq!(state.buffer().cursor_byte_index(), 4);
        assert_eq!(state.vertical_scroll(), 0);
    }

    #[test]
    fn visual_movement_and_selection_share_wrapped_geometry() {
        let mut state = TextInputState::new(TextEditBuffer::from_text("abcdefghi"));
        state.set_content_area(Rect::new(0, 0, 3, 3), &TextInputPolicy::chat_composer());
        state.buffer_mut().move_cursor(TextMotion::Absolute(1));
        move_visual_cursor(&mut state, 1, SelectionMode::Move);
        assert_eq!(state.buffer().cursor_byte_index(), 4);
        extend_visual_selection(&mut state, 1);
        assert_eq!(state.buffer().cursor_byte_index(), 7);
        assert!(state.buffer().selection().is_some());
        move_visual_cursor(&mut state, -1, SelectionMode::Move);
        assert_eq!(state.buffer().cursor_byte_index(), 4);
        assert!(state.buffer().selection().is_none());
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
    fn trailing_exclusion_reclaims_full_rows_and_keeps_scroll_stable() {
        let policy = TextInputPolicy::chat_composer();
        let mut state =
            TextInputState::new(TextEditBuffer::from_text("abcdefghijklmnopqrstuvwxyz"));
        state.viewport_height = 2;
        state.exclusion_columns = 5;
        state.set_content_area(Rect::new(0, 0, 12, 2), &policy);
        let scroll = state.vertical_scroll();
        for _ in 0..8 {
            state.sync_scroll_to_cursor(&policy);
            assert_eq!(state.vertical_scroll(), scroll);
            let projection = state.wrapped_layout(12);
            let row = projection
                .cursor
                .row
                .saturating_sub(usize::try_from(scroll).unwrap());
            assert!(row < 2);
            if row == 1 {
                assert!(projection.cursor.col < 7);
            }
        }
        state.buffer_mut().clear();
        state.buffer_mut().insert_str("abc");
        state.sync_scroll_to_cursor(&policy);
        assert_eq!(state.vertical_scroll(), 0);
        assert_eq!(state.rows_with_trailing_exclusion(12, 5, 8), 1);
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);
        assert_eq!(state.wrapped_layout(20).lines, ["abc"]);
    }

    #[test]
    fn trailing_exclusion_uses_one_row_until_text_needs_another() {
        let short = TextInputState::new(TextEditBuffer::from_text("hello"));
        assert_eq!(short.rows_with_trailing_exclusion(12, 5, 8), 1);
        let long = TextInputState::new(TextEditBuffer::from_text("abcdefghijk"));
        assert_eq!(long.rows_with_trailing_exclusion(12, 5, 8), 2);
        assert_eq!(long.rows_with_trailing_exclusion(20, 5, 8), 1);
        assert_eq!(long.rows_with_trailing_exclusion(12, 5, 1), 1);
    }

    #[test]
    fn trailing_exclusion_shares_paint_cursor_and_pointer_projection() {
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text(
            "abcdefghijklmn",
        )));
        let component = TextInputComponent::new("editor", &state, &policy)
            .trailing_exclusion(5)
            .focused(true);
        let layout = component.layout(Constraints::tight(Size::new(12, 2)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("abcdefghijkl")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("mn          ")
        );
        let mut cx = EventCx::with_clip(&layout, Rect::new(0, 0, 12, 2));
        component.event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
            &layout,
            &mut cx,
        );
        assert_eq!(state.borrow().buffer().cursor_byte_index(), 13);
        let retained = state.borrow();
        let wrapped = retained.wrapped_layout(12);
        assert_eq!(
            wrapped.cursor,
            wrapped.cursor_for_byte_index(retained.buffer().text(), 13)
        );
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

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TextInputSnapshot<B = TextEditBuffer> {
    buffer: B,
    content_area: (u16, u16, u16, u16),
    logical_width: u64,
    viewport_height: u64,
    pointer_offset: (usize, usize),
    scroll: ScrollViewState,
    active: SelectionGranularity,
    last_click: Option<ClickSnapshot>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClickSnapshot {
    x: u16,
    y: u16,
    age: Duration,
    count: u8,
}

#[cfg(test)]
mod snapshot_tests {
    use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};

    use super::{MouseClickState, SelectionGranularity, TextInputState};

    #[test]
    fn editor_snapshot_preserves_selection_scroll_and_pending_drag() {
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello world"));
        state.buffer.move_cursor(TextMotion::Left);
        state
            .buffer
            .move_cursor_with_selection(TextMotion::Left, SelectionMode::Extend);
        state.scroll.set_vertical_offset(7);
        state.mouse_selection.active = SelectionGranularity::Word;
        state.mouse_selection.last_click = Some(MouseClickState {
            x: 2,
            y: 1,
            at: std::time::Instant::now(),
            count: 2,
        });
        let snapshot = state.snapshot();
        let mut rebuilt = TextInputState::default();
        rebuilt.restore_snapshot(&snapshot).unwrap();
        assert_eq!(rebuilt.buffer.text(), state.buffer.text());
        assert_eq!(
            rebuilt.buffer.cursor_byte_index(),
            state.buffer.cursor_byte_index()
        );
        assert_eq!(rebuilt.buffer.selection(), state.buffer.selection());
        assert_eq!(rebuilt.scroll, state.scroll);
        assert_eq!(rebuilt.mouse_selection.active, SelectionGranularity::Word);
        assert_eq!(rebuilt.mouse_selection.last_click.unwrap().count, 2);
        let before = rebuilt.clone();
        let mut malformed = snapshot;
        malformed["buffer"]["cursor"] = serde_json::json!(99);
        assert!(rebuilt.restore_snapshot(&malformed).is_err());
        assert_eq!(rebuilt, before);
    }

    #[test]
    fn editor_rejects_non_grapheme_cursor_and_bad_click_count() {
        let state = TextInputState::new(TextEditBuffer::from_text("e\u{301}"));
        let mut snapshot = state.snapshot();
        snapshot["buffer"]["cursor"] = serde_json::json!(1);
        let mut rebuilt = TextInputState::default();
        assert!(rebuilt.restore_snapshot(&snapshot).is_err());
        let mut snapshot = state.snapshot();
        snapshot["last_click"] =
            serde_json::json!({"x": 0, "y": 0, "age": {"secs": 0, "nanos": 0}, "count": 255});
        assert!(rebuilt.restore_snapshot(&snapshot).is_err());
    }
}
