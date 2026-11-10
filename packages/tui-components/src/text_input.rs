//! Higher-level text-input control with opt-in behavior policies.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_text_edit::keyboard::{
    DEFAULT_REDO_STROKES, DEFAULT_UNDO_STROKES, TextBindings, TextKeymap, typed_char_for_key,
};
pub use bmux_text_edit::pointer::SelectionGranularity;
use bmux_text_edit::pointer::{
    ClickPolicy, ClickRecord, ClickTracker, FIELD_CLICK_GRANULARITIES, PointerSelection,
    granularity_for_click_count,
};
use bmux_text_edit::{SelectionMode, TextDelete, TextEditAction, TextEditBuffer, TextMotion};
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

/// How a [`TextInputState`] presents its text, for collecting secrets such as
/// API keys and passwords.
///
/// Masking is opt-in through [`TextInputState::with_mask`] or
/// [`TextInputState::set_mask`]. While masked:
///
/// * Rendering, wrapping, cursor placement, layout height and pointer hit
///   mapping use the mask projection, never the secret's own glyphs or widths.
///   Newlines are masked like any other grapheme.
/// * Selection is disabled (keyboard, mouse drag and multi-click), so nothing
///   can be copied out of the field. Word and hard-line motions/deletions act on
///   the whole buffer so the cursor does not reveal word or line structure.
/// * `Debug` output and [`TextInputState::snapshot`] omit the text.
/// * The text and its undo history are zeroized on [`TextInputState::wipe`] and
///   when the state is dropped.
///
/// The application still reads the value through [`TextInputState::buffer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum TextInputMask {
    /// Render the text as typed.
    #[default]
    None,
    /// Render one glyph per grapheme. Control characters and combining marks
    /// fall back to [`TextInputMask::DEFAULT_GLYPH`].
    Mask(char),
    /// Render nothing; a focused input shows only its cursor, at the start.
    Hidden,
}

impl TextInputMask {
    /// Default mask glyph.
    pub const DEFAULT_GLYPH: char = '\u{2022}';

    /// Mask each grapheme with [`TextInputMask::DEFAULT_GLYPH`] (`•`).
    #[must_use]
    pub const fn bullets() -> Self {
        Self::Mask(Self::DEFAULT_GLYPH)
    }

    /// Return whether this mode hides the real text.
    #[must_use]
    pub const fn is_masked(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Return the glyph painted per grapheme, or `None` when unmasked or hidden.
    #[must_use]
    pub fn glyph(self) -> Option<char> {
        let Self::Mask(glyph) = self else {
            return None;
        };
        // A glyph must stay one grapheme per source grapheme: two copies must not
        // combine, and control characters must not alter the line layout.
        let pair: String = [glyph, glyph].iter().collect();
        Some(if glyph.is_control() || pair.graphemes(true).count() != 2 {
            Self::DEFAULT_GLYPH
        } else {
            glyph
        })
    }
}

/// Stateful text input data used by [`TextInputControl`].
///
/// `Debug` omits the text while [`masked`](TextInputMask).
#[derive(Clone)]
pub struct TextInputState {
    buffer: TextEditBuffer,
    mask: TextInputMask,
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

impl fmt::Debug for TextInputState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("TextInputState");
        if self.mask.is_masked() {
            debug.field("buffer", &format_args!("<redacted>"));
        } else {
            debug.field("buffer", &self.buffer);
        }
        debug
            .field("mask", &self.mask)
            .field("content_area", &self.content_area)
            .field("logical_width", &self.logical_width)
            .field("viewport_height", &self.viewport_height)
            .field("pointer_offset", &self.pointer_offset)
            .field("scroll", &self.scroll)
            .field("mouse_selection", &self.mouse_selection)
            .field("exclusion_columns", &self.exclusion_columns)
            .finish_non_exhaustive()
    }
}

impl Drop for TextInputState {
    fn drop(&mut self) {
        if self.mask.is_masked() {
            self.buffer.wipe();
        }
    }
}

// Derived measurement state must not change the meaning of editor equality.
impl PartialEq for TextInputState {
    fn eq(&self, other: &Self) -> bool {
        self.buffer == other.buffer
            && self.mask == other.mask
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
    /// A masked state records no text, cursor or pointer state.
    #[must_use]
    pub fn snapshot(&self) -> serde_json::Value {
        let redacted = TextEditBuffer::new();
        let masked = self.mask.is_masked();
        let snapshot = TextInputSnapshot {
            buffer: if masked { &redacted } else { &self.buffer },
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
            gesture: if masked {
                PointerSelection::default()
            } else {
                self.mouse_selection.gesture.clone()
            },
            last_click: self
                .mouse_selection
                .clicks
                .last_click()
                .filter(|_| !masked)
                .map(|click| ClickSnapshot {
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
    /// The mask is kept; a masked state keeps its current text and cursor and
    /// restores only viewport geometry and scroll.
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
        let is_grapheme_boundary = |index: usize| {
            index == text.len()
                || text
                    .grapheme_indices(true)
                    .any(|(offset, _)| offset == index)
        };
        for field in ["cursor", "selection_anchor"] {
            let position = buffer.get(field).ok_or("missing editor position")?;
            if field == "selection_anchor" && position.is_null() {
                continue;
            }
            let index = position
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("invalid editor position")?;
            if !is_grapheme_boundary(index) {
                return Err("editor position is not a grapheme boundary".into());
            }
        }
        let mut restored: TextInputSnapshot = crate::common::decode_state_snapshot(snapshot)?;
        if let Some(anchor) = restored.gesture.anchor()
            && (anchor.start > anchor.end
                || !is_grapheme_boundary(anchor.start)
                || !is_grapheme_boundary(anchor.end))
        {
            return Err("invalid editor gesture anchor".into());
        }
        let last_click = restored
            .last_click
            .map(|click| {
                if !(1..=ClickPolicy::MAX_COUNT).contains(&click.count)
                    || click.age > Duration::from_secs(1)
                {
                    return Err("invalid editor click state".to_owned());
                }
                let at = Instant::now()
                    .checked_sub(click.age)
                    .ok_or("invalid editor click time")?;
                Ok(ClickRecord {
                    x: click.x,
                    y: click.y,
                    at,
                    count: click.count,
                })
            })
            .transpose()?;
        let mask = self.mask;
        let (buffer, gesture, last_click) = if mask.is_masked() {
            restored.buffer.wipe();
            (
                std::mem::take(&mut self.buffer),
                PointerSelection::default(),
                None,
            )
        } else {
            (
                std::mem::take(&mut restored.buffer),
                std::mem::take(&mut restored.gesture),
                last_click,
            )
        };
        *self = Self {
            buffer,
            mask,
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
                clicks: ClickTracker::from_last_click(last_click),
                gesture,
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
            mask: TextInputMask::None,
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

    /// Return this state with a presentation mask, for secret entry.
    ///
    /// ```
    /// use bmux_text_edit::TextEditBuffer;
    /// use bmux_tui_components::text_input::{TextInputMask, TextInputState};
    ///
    /// let state = TextInputState::new(TextEditBuffer::new()).with_mask(TextInputMask::bullets());
    /// assert!(state.is_masked());
    /// ```
    #[must_use]
    pub fn with_mask(mut self, mask: TextInputMask) -> Self {
        self.set_mask(mask);
        self
    }

    /// Change the presentation mask. Masking clears any selection and pointer
    /// gesture; see [`TextInputMask`] for the masked behavior.
    pub fn set_mask(&mut self, mask: TextInputMask) {
        if mask.is_masked() {
            self.buffer.clear_selection();
            self.mouse_selection = MouseSelectionState::default();
        }
        self.mask = mask;
        self.buffer.reset_preferred_visual_column();
        *self.wrapped.get_mut() = None;
    }

    /// Return the presentation mask.
    #[must_use]
    pub const fn mask(&self) -> TextInputMask {
        self.mask
    }

    /// Return whether the text is masked.
    #[must_use]
    pub const fn is_masked(&self) -> bool {
        self.mask.is_masked()
    }

    /// Clear the text, selection, pointer gesture and undo history, overwriting
    /// the previous text and its history snapshots with zeros. The mask is kept.
    pub fn wipe(&mut self) {
        self.buffer.wipe();
        self.mouse_selection = MouseSelectionState::default();
        *self.wrapped.get_mut() = None;
        self.scroll = ScrollViewState::new();
    }

    /// The buffer that wrapping, painting and pointer geometry are measured on:
    /// the real buffer, or a glyph projection with the cursor at the same grapheme.
    fn projection(&self) -> Cow<'_, TextEditBuffer> {
        match self.mask {
            TextInputMask::None => Cow::Borrowed(&self.buffer),
            TextInputMask::Hidden => Cow::Owned(TextEditBuffer::new()),
            TextInputMask::Mask(_) => {
                let glyph = self.mask.glyph().unwrap_or(TextInputMask::DEFAULT_GLYPH);
                let count = self.buffer.text().graphemes(true).count();
                let mut projection = TextEditBuffer::from_text(
                    std::iter::repeat_n(glyph, count).collect::<String>(),
                );
                projection.move_cursor(TextMotion::Absolute(
                    self.buffer
                        .cursor_grapheme_index()
                        .saturating_mul(glyph.len_utf8()),
                ));
                Cow::Owned(projection)
            }
        }
    }

    /// Map a byte index in [`Self::projection`] back to the real buffer. Hidden
    /// text has no geometry to map from.
    fn source_byte_index(&self, projected: usize) -> Option<usize> {
        match self.mask {
            TextInputMask::None => Some(projected),
            TextInputMask::Hidden => None,
            TextInputMask::Mask(_) => {
                let glyph = self.mask.glyph().unwrap_or(TextInputMask::DEFAULT_GLYPH);
                let text = self.buffer.text();
                Some(
                    text.grapheme_indices(true)
                        .nth(projected / glyph.len_utf8())
                        .map_or(text.len(), |(offset, _)| offset),
                )
            }
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
        let projection = self.projection();
        let base = projection
            .wrapped_layout(usize::from(width))
            .lines
            .len()
            .max(1);
        let rows = base.min(usize::from(maximum));
        if base > usize::from(maximum) || columns == 0 {
            return u16::try_from(rows).unwrap_or(maximum);
        }
        let layout = projection.wrapped_layout_with_exclusion(
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
        // The cache is keyed by the real cursor; masked layouts are measured on
        // the projection, whose cursor sits at the same grapheme.
        if stale {
            *self.wrapped.borrow_mut() = Some((
                width,
                self.buffer.cursor_byte_index(),
                self.projection().wrapped_layout_with_exclusion(
                    usize::try_from(width.max(1)).unwrap_or(usize::MAX),
                    exclusion,
                ),
            ));
        } else if let Some((_, cursor, layout)) = self.wrapped.borrow_mut().as_mut()
            && *cursor != self.buffer.cursor_byte_index()
        {
            *cursor = self.buffer.cursor_byte_index();
            let projection = self.projection();
            layout.cursor =
                layout.cursor_for_byte_index(projection.text(), projection.cursor_byte_index());
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
        self.mouse_selection.gesture.is_active()
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
    text_bindings: Option<&'policy TextBindings>,
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
            text_bindings: None,
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

    /// Route editing keys through a rebindable action table.
    ///
    /// See [`TextInputControl::with_text_bindings`].
    #[must_use]
    pub const fn with_text_bindings(mut self, bindings: &'policy TextBindings) -> Self {
        self.text_bindings = Some(bindings);
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
        // Masked layouts depend only on the projection; never hash the secret.
        state.mask().hash(&mut layout);
        state.projection().text().hash(&mut layout);
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
        state.buffer().is_empty().hash(&mut paint);
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
        let projection = state.projection();
        let mut input = TextInput::new(&projection)
            .wrapped_layout(&wrapped)
            .trailing_exclusion(self.trailing_exclusion)
            .id(self.id.clone())
            .style(self.style)
            .selection_style(self.selection_style)
            .placeholder_style(self.placeholder_style)
            .cursor_visible(self.focused && !self.disabled)
            .vertical_scroll(usize::try_from(state.vertical_scroll()).unwrap_or(usize::MAX));
        // A hidden projection is empty even when text exists; the placeholder
        // follows the real buffer.
        if let Some(placeholder) = self.placeholder
            && state.buffer().is_empty()
        {
            input = input.placeholder(placeholder);
        }
        input.paint(layout, cx);
        drop(projection);
        drop(wrapped);
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
        let mut control =
            TextInputControl::new(self.policy).with_history_bindings(self.history_bindings);
        if let Some(bindings) = self.text_bindings {
            control = control.with_text_bindings(bindings);
        }
        match control.handle_event(&mut state, event) {
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
            undo: DEFAULT_UNDO_STROKES,
            redo: DEFAULT_REDO_STROKES,
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
    text_bindings: Option<&'policy TextBindings>,
}

impl<'policy> TextInputControl<'policy> {
    /// Create a control using `policy`.
    #[must_use]
    pub const fn new(policy: &'policy TextInputPolicy) -> Self {
        Self {
            policy,
            history_bindings: TextInputHistoryBindings::standard(),
            text_bindings: None,
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

    /// Route editing keys through a rebindable action table.
    ///
    /// The table becomes authoritative for every non-insert editing key,
    /// including undo/redo: [`handle_key`](Self::handle_key) resolves
    /// [`TextEditAction::Undo`]/[`TextEditAction::Redo`] from it instead of the
    /// history bindings. Typed printable characters are still inserted when a
    /// stroke is unbound, Enter keeps its [`KeyboardPolicy`] behavior, vertical
    /// moves keep edge signals and soft-wrap-aware movement, and
    /// [`KeyboardPolicy::selection_keys`] still gates the selection actions.
    /// [`TextBindings::defaults`] for the policy keymap reproduces the
    /// behavior without a table.
    #[must_use]
    pub const fn with_text_bindings(mut self, bindings: &'policy TextBindings) -> Self {
        self.text_bindings = Some(bindings);
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
        let history_action = self.text_bindings.map_or_else(
            || self.history_bindings.action_for_key(stroke),
            |bindings| match bindings.resolve(stroke) {
                Some(TextEditAction::Undo) => Some(TextInputHistoryAction::Undo),
                Some(TextEditAction::Redo) => Some(TextInputHistoryAction::Redo),
                _ => None,
            },
        );
        self.handle_key_with_history_action(state, stroke, history_action)
    }

    /// Handle a stroke with the authoritative history result from a semantic adapter.
    ///
    /// `None` suppresses all default history bindings (including undo/redo entries
    /// of a [`TextBindings`] table), while ordinary editing still uses the
    /// configured text keymap or table. Disabled keyboard policy overrides actions.
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
        if let Some(bindings) = self.text_bindings {
            return self.handle_bound_key(state, stroke, bindings);
        }
        if self.selection_keys(state)
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
        let command = if state.is_masked() {
            masked_command(command)
        } else {
            command
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

    fn handle_bound_key(
        &self,
        state: &mut TextInputState,
        stroke: KeyStroke,
        bindings: &TextBindings,
    ) -> TextInputOutcome {
        let action = match bindings.resolve(stroke) {
            // History is resolved by the caller before reaching this point.
            Some(TextEditAction::Undo | TextEditAction::Redo) => {
                return TextInputOutcome::Ignored;
            }
            Some(action) if action.is_selection() && !self.selection_keys(state) => {
                return TextInputOutcome::Ignored;
            }
            Some(action) => action,
            None => {
                let Some(ch) = typed_char_for_key(stroke) else {
                    return TextInputOutcome::Ignored;
                };
                state.buffer_mut().insert_char(ch);
                state.sync_scroll_to_cursor(self.policy);
                return TextInputOutcome::Edited;
            }
        };
        match action.motion() {
            Some((motion @ (TextMotion::VisualUp | TextMotion::VisualDown), mode)) => {
                let delta = if motion == TextMotion::VisualUp {
                    -1
                } else {
                    1
                };
                if mode == SelectionMode::Move
                    && let Some(outcome) = self.edge_outcome(state, delta)
                {
                    return outcome;
                }
                move_visual_cursor(state, delta, mode);
            }
            Some((motion, mode)) if state.is_masked() => {
                state
                    .buffer
                    .move_cursor_with_selection(masked_motion(motion), mode);
            }
            Some((motion, mode)) => state.buffer.move_cursor_with_selection(motion, mode),
            None if action == TextEditAction::SelectAll => state.buffer.select_all(),
            None => match action.deletion() {
                Some(deletion) if state.is_masked() => {
                    state.buffer_mut().delete(masked_delete(deletion));
                }
                _ => {
                    state.buffer_mut().apply_action(action);
                }
            },
        }
        state.sync_scroll_to_cursor(self.policy);
        TextInputOutcome::Edited
    }

    /// Whether selection keys apply; masked text never selects.
    const fn selection_keys(&self, state: &TextInputState) -> bool {
        self.policy.keyboard.selection_keys && !state.is_masked()
    }

    /// Handle one mouse event.
    pub fn handle_mouse(&self, state: &mut TextInputState, mouse: MouseEvent) -> TextInputOutcome {
        if !self.policy.mouse.enabled {
            return TextInputOutcome::Ignored;
        }
        if state.is_masked() {
            return self.handle_masked_mouse(state, mouse);
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click_to_cursor => {
                self.handle_mouse_down(state, mouse)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.policy.mouse.drag_selection => {
                self.handle_mouse_drag(state, mouse)
            }
            MouseEventKind::Up(MouseButton::Left) if state.mouse_selection_active() => {
                state.mouse_selection.gesture.end();
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
        match stroke.key {
            KeyCode::Up => self.edge_outcome(state, -1),
            KeyCode::Down => self.edge_outcome(state, 1),
            KeyCode::Char(_)
            | KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Escape
            | KeyCode::Space
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

    /// Return the edge signal for a vertical move of `delta` rows, if enabled.
    fn edge_outcome(&self, state: &TextInputState, delta: isize) -> Option<TextInputOutcome> {
        let layout = state.wrapped_layout(state.logical_width);
        if delta < 0 && layout.cursor.row == 0 && self.policy.edge.up_at_first_row {
            return Some(TextInputOutcome::EdgeUp);
        }
        if delta > 0
            && layout.cursor.row.saturating_add(1) >= layout.lines.len()
            && self.policy.edge.down_at_last_row
        {
            return Some(TextInputOutcome::EdgeDown);
        }
        None
    }

    /// Masked text supports click-to-cursor only: no drag, word or line selection.
    fn handle_masked_mouse(
        &self,
        state: &mut TextInputState,
        mouse: MouseEvent,
    ) -> TextInputOutcome {
        state.mouse_selection.gesture.end();
        if mouse.kind != MouseEventKind::Down(MouseButton::Left)
            || !self.policy.mouse.click_to_cursor
        {
            return TextInputOutcome::Ignored;
        }
        let Some((row, col)) = mouse_wrapped_position(state, mouse) else {
            return TextInputOutcome::Ignored;
        };
        let projected = state
            .wrapped_layout(state.logical_width)
            .byte_index_for_position(row, col);
        if let Some(source) = state.source_byte_index(projected) {
            state.buffer.move_cursor(TextMotion::Absolute(source));
        }
        state.sync_scroll_to_cursor(self.policy);
        TextInputOutcome::Redraw
    }

    fn handle_mouse_down(&self, state: &mut TextInputState, mouse: MouseEvent) -> TextInputOutcome {
        let Some((row, col)) = mouse_wrapped_position(state, mouse) else {
            state.mouse_selection.gesture.end();
            return TextInputOutcome::Ignored;
        };
        let policy = self.policy.mouse;
        let count = state.mouse_selection.clicks.register(
            mouse.position.x,
            mouse.position.y,
            Instant::now(),
            &policy.click,
        );
        let granularity = granularity_for_click_count(policy.click_granularities, count);
        let extend = matches!(policy.shift_click, ShiftClick::Extend) && mouse.modifiers.shift;
        let byte_index = state
            .wrapped_layout(state.logical_width)
            .byte_index_for_position(row, col);
        let selection = &mut state.mouse_selection;
        selection
            .gesture
            .begin(&mut state.buffer, byte_index, granularity, extend);
        if !policy.drag_selection {
            selection.gesture.end();
        }
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
        state
            .mouse_selection
            .gesture
            .drag(&mut state.buffer, byte_index);
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

    /// Single-row secret field (API key, password): Enter submits, typing,
    /// paste and click-to-cursor work, and nothing can be selected or copied.
    /// Pair it with a [`TextInputMask`] on the state.
    #[must_use]
    pub const fn secret_field() -> Self {
        Self {
            outer_selection: TextInputOuterSelectionPolicy::Isolated,
            keyboard: KeyboardPolicy {
                shift_enter: None,
                selection_keys: false,
                ..KeyboardPolicy::chat_composer()
            },
            mouse: MousePolicy {
                drag_selection: false,
                edge_scroll: DragEdgeScroll::Disabled,
                ..MousePolicy::text_selection()
            }
            .click_granularities(&[]),
            viewport: ViewportPolicy {
                auto_scroll_to_cursor: true,
                min_rows: 1,
                max_rows: Some(1),
            },
            edge: EdgePolicy::disabled(),
        }
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
    /// Selection granularity for the 1st, 2nd, 3rd, ... consecutive click.
    /// Counts past the end reuse the last entry; empty selects by character.
    pub click_granularities: &'static [SelectionGranularity],
    /// Multi-click timing/distance detection.
    pub click: ClickPolicy,
    /// Shift-click behavior.
    pub shift_click: ShiftClick,
}

/// Mouse-down behavior while shift is held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShiftClick {
    /// Treat shift-click like a plain click, replacing the selection.
    #[default]
    Replace,
    /// Keep the selection anchor and extend the selection to the click.
    Extend,
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
            click_granularities: &[],
            click: ClickPolicy::with_max_count(1),
            shift_click: ShiftClick::Replace,
        }
    }

    /// Text-selection mouse behavior: 1 click = character, 2 = word, 3 = all.
    #[must_use]
    pub const fn text_selection() -> Self {
        Self {
            enabled: true,
            click_to_cursor: true,
            drag_selection: true,
            edge_scroll: DragEdgeScroll::Enabled,
            click_granularities: FIELD_CLICK_GRANULARITIES,
            click: ClickPolicy::with_max_count(3),
            shift_click: ShiftClick::Replace,
        }
    }

    /// Use `sequence` as the click-count to granularity mapping, counting up to
    /// its length (for example
    /// [`EDITOR_CLICK_GRANULARITIES`](bmux_text_edit::pointer::EDITOR_CLICK_GRANULARITIES)
    /// for 1 = character, 2 = word, 3 = line, 4 = all).
    #[must_use]
    pub const fn click_granularities(mut self, sequence: &'static [SelectionGranularity]) -> Self {
        self.click_granularities = sequence;
        self.click.max_count = if sequence.len() > u8::MAX as usize {
            u8::MAX
        } else {
            #[allow(clippy::cast_possible_truncation)] // Bounded by the branch above.
            {
                sequence.len() as u8
            }
        };
        self
    }

    /// Set shift-click behavior.
    #[must_use]
    pub const fn shift_click(mut self, behavior: ShiftClick) -> Self {
        self.shift_click = behavior;
        self
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MouseSelectionState {
    clicks: ClickTracker,
    gesture: PointerSelection,
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
    if state.is_masked() {
        // Masked rows hold one glyph per grapheme; move on the projection and
        // map the target grapheme back to the real buffer.
        let projected = {
            let layout = state.wrapped_layout(state.logical_width);
            let row = layout
                .cursor
                .row
                .saturating_add_signed(delta)
                .min(layout.lines.len().saturating_sub(1));
            layout.byte_index_for_position(row, layout.cursor.col)
        };
        if let Some(source) = state.source_byte_index(projected) {
            state.buffer.move_cursor(TextMotion::Absolute(source));
        }
        return;
    }
    drop(state.wrapped_layout(state.logical_width));
    let cached = state.wrapped.borrow();
    let layout = &cached.as_ref().expect("measured projection").2;
    state.buffer.move_cursor_in_layout(layout, delta, selection);
}

/// Masked word and hard-line motions span the whole buffer so the cursor does
/// not reveal where spaces, punctuation or newlines are.
const fn masked_motion(motion: TextMotion) -> TextMotion {
    match motion {
        TextMotion::WordLeft | TextMotion::LineStart => TextMotion::Start,
        TextMotion::WordRight | TextMotion::LineEnd => TextMotion::End,
        motion => motion,
    }
}

/// Masked word and hard-line deletions span the whole buffer, like motions.
const fn masked_delete(delete: TextDelete) -> TextDelete {
    match delete {
        TextDelete::WordBackward | TextDelete::ToLineStart => TextDelete::ToStart,
        TextDelete::WordForward | TextDelete::ToLineEnd => TextDelete::ToEnd,
        delete => delete,
    }
}

fn masked_command(command: bmux_text_edit::TextEditCommand) -> bmux_text_edit::TextEditCommand {
    match command {
        bmux_text_edit::TextEditCommand::Move(motion) => {
            bmux_text_edit::TextEditCommand::Move(masked_motion(motion))
        }
        bmux_text_edit::TextEditCommand::Delete(delete) => {
            bmux_text_edit::TextEditCommand::Delete(masked_delete(delete))
        }
        command => command,
    }
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

    fn modifier_combinations() -> impl Iterator<Item = Modifiers> {
        (0_u8..16).map(|bits| Modifiers {
            ctrl: bits & 1 != 0,
            alt: bits & 2 != 0,
            shift: bits & 4 != 0,
            super_key: bits & 8 != 0,
            ..Modifiers::NONE
        })
    }

    fn parity_keys() -> Vec<KeyCode> {
        let mut keys: Vec<KeyCode> = ('a'..='z').map(KeyCode::Char).collect();
        keys.extend([
            KeyCode::Char('1'),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Escape,
            KeyCode::Space,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::F(1),
        ]);
        keys
    }

    type Observed = (
        TextInputOutcome,
        String,
        usize,
        Option<bmux_text_edit::TextSelection>,
        bool,
        bool,
    );

    fn observe(
        control: &TextInputControl<'_>,
        mut state: TextInputState,
        stroke: KeyStroke,
    ) -> Observed {
        let outcome = control.handle_key(&mut state, stroke);
        let buffer = state.buffer();
        (
            outcome,
            buffer.text().to_string(),
            buffer.cursor_byte_index(),
            buffer.selection(),
            buffer.can_undo(),
            buffer.can_redo(),
        )
    }

    fn parity_states(policy: &TextInputPolicy) -> Vec<TextInputState> {
        let mut states = Vec::new();
        for cursor in [0, 3, 14, 31] {
            let mut state =
                TextInputState::new(TextEditBuffer::from_text("alpha beta\ngamma delta epsilon"));
            state.set_content_area(Rect::new(0, 0, 8, 3), policy);
            state.buffer_mut().insert_str("!");
            state.buffer_mut().undo();
            state.buffer_mut().move_cursor(TextMotion::Absolute(cursor));
            states.push(state.clone());
            state
                .buffer_mut()
                .move_cursor_with_selection(TextMotion::Left, SelectionMode::Extend);
            states.push(state);
        }
        states
    }

    #[test]
    fn default_text_bindings_match_builtin_key_handling() {
        use bmux_text_edit::TextBoundaryPolicy;
        use bmux_text_edit::keyboard::TextInputProfile;

        for profile in [
            TextInputProfile::Readline,
            TextInputProfile::MacTerminal,
            TextInputProfile::WindowsTerminal,
        ] {
            for boundary_policy in [TextBoundaryPolicy::Buffer, TextBoundaryPolicy::Line] {
                for edges in [false, true] {
                    let mut policy = TextInputPolicy::chat_composer();
                    policy.keyboard.keymap = TextKeymap {
                        profile,
                        boundary_policy,
                    };
                    policy.edge = EdgePolicy {
                        up_at_first_row: edges,
                        down_at_last_row: edges,
                    };
                    let bindings = policy.keyboard.keymap.bindings();
                    let builtin = TextInputControl::new(&policy);
                    let bound = TextInputControl::new(&policy).with_text_bindings(&bindings);
                    for state in parity_states(&policy) {
                        for code in parity_keys() {
                            for modifiers in modifier_combinations() {
                                if profile == TextInputProfile::WindowsTerminal
                                    && modifiers.ctrl
                                    && modifiers.super_key
                                    && !modifiers.alt
                                {
                                    // Lenient legacy combination outside the table.
                                    continue;
                                }
                                let stroke = KeyStroke::with_modifiers(code, modifiers);
                                assert_eq!(
                                    observe(&bound, state.clone(), stroke),
                                    observe(&builtin, state.clone(), stroke),
                                    "{profile:?} {boundary_policy:?} edges={edges} {stroke:?} \
                                     cursor={}",
                                    state.buffer().cursor_byte_index()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn text_bindings_rebind_unbind_and_gate_selection() {
        let ctrl = |ch| {
            KeyStroke::with_modifiers(
                KeyCode::Char(ch),
                Modifiers {
                    ctrl: true,
                    ..Modifiers::NONE
                },
            )
        };
        let mut bindings = TextBindings::default();
        bindings.set(TextEditAction::MoveLeft, &[]);
        bindings.set(TextEditAction::MoveWordLeft, &[key(KeyCode::F(5))]);
        bindings.unbind_stroke(ctrl('a'));
        bindings.set(TextEditAction::SelectAll, &[ctrl('a')]);
        bindings.set(TextEditAction::Undo, &[key(KeyCode::F(7))]);
        bindings.set(TextEditAction::Clear, &[key(KeyCode::Escape)]);
        assert_eq!(bindings.conflicts(), Vec::new());

        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy).with_text_bindings(&bindings);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello world"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::Left)),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().cursor_byte_index(), 11);
        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::F(5))),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().cursor_byte_index(), 6);
        assert_eq!(
            control.handle_key(&mut state, ctrl('a')),
            TextInputOutcome::Edited
        );
        assert_eq!(
            state.buffer().selected_text().as_deref(),
            Some("hello world")
        );

        // Typed characters stay implicit and replace the selection.
        control.handle_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(state.buffer().text(), "x");

        // Undo moved to F7; the default ctrl-Z no longer undoes or inserts.
        assert_eq!(
            control.handle_key(&mut state, ctrl('z')),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().text(), "x");
        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::F(7))),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "hello world");

        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::Escape)),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "");

        // The semantic adapter stays authoritative for history.
        state.buffer_mut().insert_str("abc");
        assert_eq!(
            control.handle_key_with_history_action(&mut state, key(KeyCode::F(7)), None),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().text(), "abc");

        let mut no_selection = TextInputPolicy::chat_composer();
        no_selection.keyboard.selection_keys = false;
        let gated = TextInputControl::new(&no_selection).with_text_bindings(&bindings);
        assert_eq!(
            gated.handle_key(&mut state, ctrl('a')),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().selection(), None);
    }

    #[test]
    fn component_forwards_text_bindings() {
        let policy = TextInputPolicy::chat_composer();
        let mut bindings = TextBindings::default();
        bindings.set(TextEditAction::DeleteToStart, &[key(KeyCode::F(9))]);
        let state = RefCell::new(TextInputState::new(TextEditBuffer::from_text("abc")));
        let component = TextInputComponent::new("editor", &state, &policy)
            .with_text_bindings(&bindings)
            .focused(true);
        let layout = LayoutNode::leaf("editor".into(), LogicalSize::new(8, 1));
        assert_eq!(
            component.event(
                &Event::Key(key(KeyCode::F(9))),
                &layout,
                &mut EventCx::new(&layout)
            ),
            EventOutcome::Redraw
        );
        assert_eq!(state.borrow().buffer().text(), "");
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
    fn word_drag_keeps_anchor_word_in_both_directions() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("alpha beta gamma"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        for _ in 0..2 {
            let _ = control.handle_mouse(
                &mut state,
                mouse(MouseEventKind::Down(MouseButton::Left), 7, 0),
            );
        }
        assert_eq!(state.buffer().selected_text(), Some("beta".to_string()));
        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 13, 0),
        );
        assert_eq!(
            state.buffer().selected_text(),
            Some("beta gamma".to_string())
        );
        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 1, 0),
        );
        assert_eq!(
            state.buffer().selected_text(),
            Some("alpha beta".to_string())
        );
        assert_eq!(state.buffer().cursor_byte_index(), 0);
        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 1, 0),
        );
        assert!(!state.mouse_selection_active());
    }

    #[test]
    fn editor_click_granularities_select_line_then_all() {
        let mut policy = TextInputPolicy::chat_composer();
        policy.mouse = MousePolicy::text_selection()
            .click_granularities(bmux_text_edit::pointer::EDITOR_CLICK_GRANULARITIES);
        assert_eq!(policy.mouse.click.max_count, 4);
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text(
            "one two
three",
        ));
        state.set_content_area(Rect::new(0, 0, 20, 2), &policy);

        let click = |state: &mut TextInputState| {
            let _ =
                control.handle_mouse(state, mouse(MouseEventKind::Down(MouseButton::Left), 5, 0));
        };
        click(&mut state);
        click(&mut state);
        assert_eq!(state.buffer().selected_text(), Some("two".to_string()));
        click(&mut state);
        assert_eq!(
            state.buffer().selected_text(),
            Some("one two\n".to_string())
        );
        let _ = control.handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 2, 1),
        );
        assert_eq!(
            state.buffer().selected_text(),
            Some("one two\nthree".to_string())
        );
        click(&mut state);
        assert_eq!(
            state.buffer().selected_text(),
            Some("one two\nthree".to_string())
        );
        click(&mut state);
        assert_eq!(
            state.buffer().selected_text(),
            Some("one two\nthree".to_string()),
            "counts past the sequence stay at the last granularity"
        );
    }

    #[test]
    fn shift_click_extends_only_when_enabled() {
        let shift_down = |x| MouseEvent {
            modifiers: bmux_tui::event::MouseModifiers {
                shift: true,
                ..bmux_tui::event::MouseModifiers::default()
            },
            ..mouse(MouseEventKind::Down(MouseButton::Left), x, 0)
        };
        let default_policy = TextInputPolicy::chat_composer();
        let mut extending = TextInputPolicy::chat_composer();
        extending.mouse = MousePolicy::text_selection().shift_click(ShiftClick::Extend);

        for (policy, expected) in [
            (&default_policy, None),
            (&extending, Some("llo w".to_string())),
        ] {
            let control = TextInputControl::new(policy);
            let mut state = TextInputState::new(TextEditBuffer::from_text("hello world"));
            state.set_content_area(Rect::new(0, 0, 20, 1), policy);
            let _ = control.handle_mouse(
                &mut state,
                mouse(MouseEventKind::Down(MouseButton::Left), 2, 0),
            );
            let _ = control.handle_mouse(
                &mut state,
                mouse(MouseEventKind::Up(MouseButton::Left), 2, 0),
            );
            let _ = control.handle_mouse(&mut state, shift_down(7));
            assert_eq!(state.buffer().selected_text(), expected);
        }
    }

    #[test]
    fn click_without_drag_selection_does_not_start_gesture() {
        let mut policy = TextInputPolicy::chat_composer();
        policy.mouse.drag_selection = false;
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello world"));
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);
        for _ in 0..2 {
            let _ = control.handle_mouse(
                &mut state,
                mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
            );
        }
        assert_eq!(state.buffer().selected_text(), Some("hello".to_string()));
        assert!(!state.mouse_selection_active());
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
    gesture: PointerSelection,
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

    use super::{
        ClickPolicy, ClickTracker, Instant, PointerSelection, SelectionGranularity, TextInputState,
    };

    #[test]
    fn editor_snapshot_preserves_selection_scroll_and_pending_drag() {
        let mut state = TextInputState::new(TextEditBuffer::from_text("hello world"));
        state.buffer.move_cursor(TextMotion::Left);
        state
            .buffer
            .move_cursor_with_selection(TextMotion::Left, SelectionMode::Extend);
        state.scroll.set_vertical_offset(7);
        state.mouse_selection.gesture = PointerSelection::resume(SelectionGranularity::Word, 6..11);
        let policy = ClickPolicy::default();
        let mut clicks = ClickTracker::new();
        let now = Instant::now();
        clicks.register(2, 1, now, &policy);
        clicks.register(2, 1, now, &policy);
        state.mouse_selection.clicks = clicks;
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
        assert_eq!(
            rebuilt.mouse_selection.gesture.granularity(),
            Some(SelectionGranularity::Word)
        );
        assert_eq!(rebuilt.mouse_selection.gesture.anchor(), Some(6..11));
        assert_eq!(
            rebuilt.mouse_selection.clicks.last_click().unwrap().count,
            2
        );
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
        let mut snapshot = state.snapshot();
        snapshot["gesture"] =
            serde_json::json!({"granularity": "Word", "anchor": {"start": 0, "end": 1}});
        assert!(rebuilt.restore_snapshot(&snapshot).is_err());
    }
}

#[cfg(test)]
mod mask_tests {
    use std::cell::RefCell;

    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
    use bmux_text_edit::TextEditBuffer;
    use bmux_text_edit::keyboard::TextBindings;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::{Cursor, Frame};
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::style::Style;

    use super::{
        TextEditAction, TextInputComponent, TextInputControl, TextInputMask, TextInputOutcome,
        TextInputPolicy, TextInputState,
    };

    fn masked(text: &str, mask: TextInputMask) -> TextInputState {
        TextInputState::new(TextEditBuffer::from_text(text)).with_mask(mask)
    }

    fn render(
        state: &RefCell<TextInputState>,
        policy: &TextInputPolicy,
        placeholder: Option<&str>,
        width: u16,
        height: u16,
    ) -> (Vec<String>, Option<Cursor>, u64) {
        let mut component = TextInputComponent::new("secret", state, policy).focused(true);
        if let Some(placeholder) = placeholder {
            component = component.placeholder(placeholder, Style::new());
        }
        let layout = component.layout(
            Constraints::loose(Size::new(width, height)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let cursor = frame.cursor();
        drop(frame);
        let rows = (0..height)
            .map(|row| buffer.row_symbols(row).unwrap_or_default())
            .collect();
        (rows, cursor, layout.size.height)
    }

    fn key(code: KeyCode) -> KeyStroke {
        KeyStroke::simple(code)
    }

    fn with(code: KeyCode, modifiers: Modifiers) -> KeyStroke {
        KeyStroke::with_modifiers(code, modifiers)
    }

    const SHIFT: Modifiers = Modifiers {
        shift: true,
        ..Modifiers::NONE
    };
    const CTRL: Modifiers = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };

    #[test]
    fn mask_renders_one_glyph_per_grapheme_and_never_the_secret() {
        let secret = "sk-秘密🙂 x";
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(masked(secret, TextInputMask::bullets()));
        let (rows, cursor, height) = render(&state, &policy, None, 12, 1);

        // Eight graphemes, including two wide CJK and an emoji, become eight cells.
        assert_eq!(rows[0], "••••••••    ");
        assert!(!secret.chars().any(|ch| ch != ' ' && rows[0].contains(ch)));
        assert_eq!(height, 1);
        assert_eq!(cursor, Some(Cursor::visible(Point::new(8, 0))));

        let state = RefCell::new(masked("pass", TextInputMask::Mask('*')));
        assert_eq!(render(&state, &policy, None, 6, 1).0[0], "****  ");
    }

    #[test]
    fn mask_width_and_wrapping_use_glyph_count_not_secret_width() {
        let policy = TextInputPolicy::chat_composer();
        // Six wide graphemes need two six-column rows unmasked; masked they need one.
        let plain = RefCell::new(TextInputState::new(TextEditBuffer::from_text(
            "秘密秘密秘密",
        )));
        assert_eq!(render(&plain, &policy, None, 6, 3).2, 2);
        let state = RefCell::new(masked("秘密秘密秘密", TextInputMask::bullets()));
        let (rows, cursor, height) = render(&state, &policy, None, 6, 3);
        assert_eq!(height, 1);
        assert_eq!(rows[0], "••••••");
        // The cursor follows the sixth glyph, not the twelfth display column.
        assert_eq!(cursor, Some(Cursor::visible(Point::new(6, 0))));
    }

    #[test]
    fn unusable_mask_glyphs_fall_back_to_default() {
        assert_eq!(TextInputMask::Mask('\n').glyph(), Some('•'));
        assert_eq!(TextInputMask::Mask('\u{301}').glyph(), Some('•'));
        assert_eq!(TextInputMask::Mask('#').glyph(), Some('#'));
        assert_eq!(TextInputMask::Hidden.glyph(), None);
        assert_eq!(TextInputMask::None.glyph(), None);
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(masked("a\nb", TextInputMask::Mask('\n')));
        // Newlines are masked too, so line structure is not revealed.
        let (rows, _, height) = render(&state, &policy, None, 5, 2);
        assert_eq!((rows[0].as_str(), height), ("•••  ", 1));
    }

    #[test]
    fn hidden_renders_nothing_but_the_cursor() {
        let policy = TextInputPolicy::chat_composer();
        let state = RefCell::new(masked("hunter2", TextInputMask::Hidden));
        let (rows, cursor, height) = render(&state, &policy, Some("API key"), 10, 1);
        assert_eq!(rows[0], " ".repeat(10));
        assert_eq!(height, 1);
        assert_eq!(cursor, Some(Cursor::visible(Point::new(0, 0))));

        // The placeholder follows the real (empty) buffer, not the empty projection.
        state.borrow_mut().wipe();
        let (rows, _, _) = render(&state, &policy, Some("API key"), 10, 1);
        assert_eq!(rows[0], "API key   ");
    }

    #[test]
    fn masked_editing_inserts_deletes_moves_and_pastes() {
        let policy = TextInputPolicy::secret_field();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::default().with_mask(TextInputMask::bullets());
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);
        for ch in "abc".chars() {
            assert_eq!(
                control.handle_key(&mut state, key(KeyCode::Char(ch))),
                TextInputOutcome::Edited
            );
        }
        control.handle_key(&mut state, key(KeyCode::Left));
        control.handle_key(&mut state, key(KeyCode::Backspace));
        assert_eq!(state.buffer().text(), "ac");
        assert_eq!(state.buffer().cursor_grapheme_index(), 1);
        control.handle_key(&mut state, key(KeyCode::Home));
        control.handle_key(&mut state, key(KeyCode::Char('x')));
        control.handle_key(&mut state, key(KeyCode::End));
        assert_eq!(
            control.handle_event(&mut state, &Event::Paste("1 2-🙂".to_owned())),
            TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "xac1 2-🙂");
        control.handle_key(&mut state, key(KeyCode::Delete));
        control.handle_key(&mut state, key(KeyCode::Left));
        control.handle_key(&mut state, key(KeyCode::Delete));
        assert_eq!(state.buffer().text(), "xac1 2-");
        assert_eq!(
            control.handle_key(&mut state, key(KeyCode::Enter)),
            TextInputOutcome::Submitted
        );
    }

    #[test]
    fn masked_word_motion_and_deletion_span_the_whole_secret() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = masked("two words-here", TextInputMask::bullets());
        state.set_content_area(Rect::new(0, 0, 30, 1), &policy);
        control.handle_key(&mut state, with(KeyCode::Left, CTRL));
        assert_eq!(state.buffer().cursor_byte_index(), 0);
        control.handle_key(&mut state, with(KeyCode::Right, CTRL));
        assert_eq!(state.buffer().cursor_byte_index(), 14);
        control.handle_key(&mut state, with(KeyCode::Char('w'), CTRL));
        assert_eq!(state.buffer().text(), "");
    }

    #[test]
    fn masked_input_never_selects() {
        let policy = TextInputPolicy::chat_composer();
        let mut bindings = TextBindings::default();
        bindings.unbind_stroke(with(KeyCode::Char('a'), CTRL));
        bindings.set(TextEditAction::SelectAll, &[with(KeyCode::Char('a'), CTRL)]);
        let mut state = masked("secret", TextInputMask::bullets());
        state.set_content_area(Rect::new(0, 0, 20, 1), &policy);

        let control = TextInputControl::new(&policy);
        control.handle_key(&mut state, with(KeyCode::Left, SHIFT));
        assert_eq!(state.buffer().selection(), None);
        let bound = TextInputControl::new(&policy).with_text_bindings(&bindings);
        assert_eq!(
            bound.handle_key(&mut state, with(KeyCode::Char('a'), CTRL)),
            TextInputOutcome::Ignored
        );
        assert_eq!(
            bound.handle_key(&mut state, with(KeyCode::Left, SHIFT)),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().selection(), None);

        // Double-click and drag do not select; a click only places the cursor.
        let down = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(2, 0));
        for _ in 0..3 {
            control.handle_mouse(&mut state, down);
        }
        assert_eq!(state.buffer().selection(), None);
        assert_eq!(state.buffer().cursor_byte_index(), 2);
        let drag = MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(5, 0));
        assert_eq!(
            control.handle_mouse(&mut state, drag),
            TextInputOutcome::Ignored
        );
        assert_eq!(state.buffer().selected_text(), None);
        assert!(!state.mouse_selection_active());
    }

    #[test]
    fn masking_clears_an_existing_selection() {
        let mut buffer = TextEditBuffer::from_text("secret");
        buffer.select_all();
        let state = TextInputState::new(buffer).with_mask(TextInputMask::Hidden);
        assert_eq!(state.buffer().selected_text(), None);
    }

    #[test]
    fn masked_pointer_and_vertical_motion_map_glyphs_to_graphemes() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        // Wide graphemes: glyph column 1 is the second grapheme at byte 3.
        let mut state = masked("秘密ab", TextInputMask::bullets());
        state.set_content_area(Rect::new(0, 0, 10, 1), &policy);
        let click = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 0));
        assert_eq!(
            control.handle_mouse(&mut state, click),
            TextInputOutcome::Redraw
        );
        assert_eq!(state.buffer().cursor_byte_index(), 3);

        // Ten graphemes at width 4 wrap into rows of 4/4/2 glyphs.
        let mut state = masked("ab秘密efghij", TextInputMask::bullets());
        state.set_content_area(Rect::new(0, 0, 4, 3), &policy);
        assert_eq!(state.buffer().cursor_grapheme_index(), 10);
        control.handle_key(&mut state, key(KeyCode::Up));
        assert_eq!(state.buffer().cursor_grapheme_index(), 6);
        control.handle_key(&mut state, key(KeyCode::Up));
        assert_eq!(state.buffer().cursor_grapheme_index(), 2);
        assert_eq!(state.buffer().cursor_byte_index(), 2);

        // Hidden text has no geometry: clicks do not move the cursor.
        let mut state = masked("abc", TextInputMask::Hidden);
        state.set_content_area(Rect::new(0, 0, 10, 1), &policy);
        control.handle_mouse(&mut state, click);
        assert_eq!(state.buffer().cursor_byte_index(), 3);
    }

    #[test]
    fn debug_and_snapshot_redact_masked_text() {
        let plain = TextInputState::new(TextEditBuffer::from_text("hunter2"));
        assert!(format!("{plain:?}").contains("hunter2"));
        for mask in [TextInputMask::bullets(), TextInputMask::Hidden] {
            let state = TextInputState::new(TextEditBuffer::from_text("hunter2")).with_mask(mask);
            let debug = format!("{state:?}");
            assert!(!debug.contains("hunter2"), "{debug}");
            assert!(debug.contains("<redacted>"));
            let alternate = format!("{state:#?}");
            assert!(!alternate.contains("hunter2"));

            let snapshot = state.snapshot();
            assert!(!snapshot.to_string().contains("hunter2"));
            let mut restored = state.clone();
            restored.restore_snapshot(&snapshot).expect("restore");
            assert_eq!(restored.buffer().text(), "hunter2");
            assert_eq!(restored.mask(), mask);
        }
    }

    #[test]
    fn wipe_clears_text_and_history_but_keeps_mask() {
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy);
        let mut state = TextInputState::default().with_mask(TextInputMask::bullets());
        control.handle_paste(&mut state, "hunter2");
        assert!(state.buffer().can_undo());
        state.wipe();
        assert_eq!(state.buffer().text(), "");
        assert!(!state.buffer().can_undo());
        assert!(state.is_masked());
    }

    #[test]
    fn masked_revision_does_not_depend_on_secret_content() {
        let policy = TextInputPolicy::chat_composer();
        let first = RefCell::new(masked("aaaa", TextInputMask::bullets()));
        let second = RefCell::new(masked("zzzz", TextInputMask::bullets()));
        assert_eq!(
            TextInputComponent::new("secret", &first, &policy).revision(),
            TextInputComponent::new("secret", &second, &policy).revision()
        );
    }
}
