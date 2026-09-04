//! Read-only rich text/paragraph viewer built from canonical composition.
//!
//! `TextViewComponent` composes a measured [`TextBlock`] inside a
//! [`ScrollViewComponent`], so wrapping, exact content height, viewport
//! translation, clipping, scrollbars, selection geometry, and event routing all
//! derive from one authoritative layout. Scroll state is the shared
//! caller-owned [`ScrollViewState`]; no line-oriented scroll engine remains.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId,
    LayoutMetadata, LayoutNode, LogicalSize,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Alignment, Line, Span, Text, TextWrap};
use bmux_tui::style::{Color, Style};

use crate::common::local_area_of;

use crate::scroll_view::{
    ScrollView, ScrollViewComponent, ScrollViewOutcome, ScrollViewPolicy, ScrollViewState,
};
use crate::scrollbar::ScrollbarStyles;
use crate::scrollbar_layout::ScrollbarAxisLayoutMode;
use crate::selection::{
    ComponentSelectionOutcome, ComponentSelectionPolicy, ComponentSelectionState,
    paint_component_scope,
};

/// Highlight range applied to caller-owned source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewHighlight {
    /// Source line index.
    pub line: usize,
    /// Start character offset, inclusive.
    pub start: usize,
    /// End character offset, exclusive.
    pub end: usize,
    /// Style patched onto highlighted text.
    pub style: Style,
}

impl TextViewHighlight {
    /// Create a highlight range.
    #[must_use]
    pub const fn new(line: usize, start: usize, end: usize, style: Style) -> Self {
        Self {
            line,
            start,
            end,
            style,
        }
    }
}

/// Selection range applied to caller-owned source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewSelection {
    /// Source line index.
    pub line: usize,
    /// Start character offset, inclusive.
    pub start: usize,
    /// End character offset, exclusive.
    pub end: usize,
    /// Style patched onto selected text.
    pub style: Style,
}

impl TextViewSelection {
    /// Create a selection range.
    #[must_use]
    pub const fn new(line: usize, start: usize, end: usize, style: Style) -> Self {
        Self {
            line,
            start,
            end,
            style,
        }
    }
}

/// Cursor rendering hook for read-only text views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewCursor {
    /// Source line index.
    pub line: usize,
    /// Character offset to style as cursor.
    pub column: usize,
    /// Style patched onto the cursor cell.
    pub style: Style,
}

impl TextViewCursor {
    /// Create a cursor hook.
    #[must_use]
    pub const fn new(line: usize, column: usize, style: Style) -> Self {
        Self {
            line,
            column,
            style,
        }
    }
}

/// Text-view behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TextViewPolicy {
    /// Wrapping policy.
    pub wrap: TextWrap,
    /// Horizontal alignment.
    pub alignment: Alignment,
    /// Trim trailing whitespace before rendering.
    pub trim: bool,
    /// Keyboard scrolling enabled.
    pub keyboard: bool,
    /// Mouse-wheel scrolling enabled.
    pub mouse_wheel: bool,
    /// Fill the complete component rectangle with the background style.
    pub background: bool,
    /// Integrated vertical scrollbar layout mode.
    pub vertical_scrollbar: ScrollbarAxisLayoutMode,
    /// Integrated horizontal scrollbar layout mode.
    pub horizontal_scrollbar: ScrollbarAxisLayoutMode,
}

impl TextViewPolicy {
    /// Bare render-only view.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            wrap: TextWrap::None,
            alignment: Alignment::Left,
            trim: false,
            keyboard: false,
            mouse_wheel: false,
            background: false,
            vertical_scrollbar: ScrollbarAxisLayoutMode::Hidden,
            horizontal_scrollbar: ScrollbarAxisLayoutMode::Hidden,
        }
    }

    /// Scrollable paragraph view.
    #[must_use]
    pub const fn scrollable() -> Self {
        Self {
            wrap: TextWrap::Word,
            alignment: Alignment::Left,
            trim: true,
            keyboard: true,
            mouse_wheel: true,
            background: false,
            vertical_scrollbar: ScrollbarAxisLayoutMode::Hidden,
            horizontal_scrollbar: ScrollbarAxisLayoutMode::Hidden,
        }
    }

    /// Return this policy with integrated vertical scrollbar mode changed.
    #[must_use]
    pub const fn vertical_scrollbar(mut self, vertical_scrollbar: ScrollbarAxisLayoutMode) -> Self {
        self.vertical_scrollbar = vertical_scrollbar;
        self
    }

    /// Return this policy with integrated horizontal scrollbar mode changed.
    #[must_use]
    pub const fn horizontal_scrollbar(
        mut self,
        horizontal_scrollbar: ScrollbarAxisLayoutMode,
    ) -> Self {
        self.horizontal_scrollbar = horizontal_scrollbar;
        self
    }

    /// Shared scroll-view policy derived from this text-view policy.
    #[must_use]
    pub const fn scroll_view_policy(self) -> ScrollViewPolicy {
        ScrollViewPolicy {
            keyboard: self.keyboard,
            mouse_wheel: self.mouse_wheel,
            vertical_scrollbar: self.vertical_scrollbar,
            horizontal_scrollbar: if matches!(self.wrap, TextWrap::None) {
                self.horizontal_scrollbar
            } else {
                ScrollbarAxisLayoutMode::Hidden
            },
            wheel_rows: 1,
        }
    }
}

impl Default for TextViewPolicy {
    fn default() -> Self {
        Self::scrollable()
    }
}

/// Text-view visual styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextViewStyles {
    /// Text style.
    pub text: Style,
    /// Empty-content style.
    pub empty: Style,
    /// Background fill style.
    pub background: Style,
    /// Integrated scrollbar styles.
    pub scrollbar: ScrollbarStyles,
}

impl Default for TextViewStyles {
    fn default() -> Self {
        Self {
            text: Style::new().fg(Color::White),
            empty: Style::new().fg(Color::BrightBlack),
            background: Style::new(),
            scrollbar: ScrollbarStyles::default(),
        }
    }
}

/// Read-only rich text/paragraph viewer over caller-owned lines.
///
/// The component measures the exact wrapped content height at the constrained
/// width, paints the complete rectangle through the scoped paint context,
/// registers one scroll region plus integrated scrollbars, and routes events
/// through the shared [`ScrollView`] controller. Scroll state remains
/// caller-owned through an interior-mutable `Cell`.
pub struct TextViewComponent<'a, 'state> {
    id: LayoutId,
    lines: &'a [Line],
    highlights: &'a [TextViewHighlight],
    selection: Option<TextViewSelection>,
    cursor: Option<TextViewCursor>,
    policy: TextViewPolicy,
    styles: TextViewStyles,
    empty: &'a str,
    state: &'state Cell<ScrollViewState>,
}

impl<'a, 'state> TextViewComponent<'a, 'state> {
    /// Create a text view with stable identity and caller-owned scroll state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        lines: &'a [Line],
        state: &'state Cell<ScrollViewState>,
    ) -> Self {
        Self {
            id: id.into(),
            lines,
            highlights: &[],
            selection: None,
            cursor: None,
            policy: TextViewPolicy::scrollable(),
            styles: TextViewStyles {
                text: Style::new(),
                empty: Style::new(),
                background: Style::new(),
                scrollbar: ScrollbarStyles::default(),
            },
            empty: "No content",
            state,
        }
    }

    /// Set highlighted source text ranges.
    #[must_use]
    pub const fn highlights(mut self, highlights: &'a [TextViewHighlight]) -> Self {
        self.highlights = highlights;
        self
    }

    /// Set selected source text range.
    #[must_use]
    pub const fn selection(mut self, selection: Option<TextViewSelection>) -> Self {
        self.selection = selection;
        self
    }

    /// Set read-only cursor rendering hook.
    #[must_use]
    pub const fn cursor(mut self, cursor: Option<TextViewCursor>) -> Self {
        self.cursor = cursor;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TextViewPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TextViewStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set empty-content message.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.empty = empty;
        self
    }

    /// Stable identity of the inner scroll viewport.
    #[must_use]
    pub fn viewport_id(&self) -> LayoutId {
        LayoutId::new(format!("{}.viewport", self.id.as_str()))
    }

    /// Stable identity of the measured text content.
    #[must_use]
    pub fn content_id(&self) -> LayoutId {
        LayoutId::new(format!("{}.content", self.id.as_str()))
    }

    /// Shared scroll controller configured from this view's policy and styles.
    #[must_use]
    pub const fn scroll_view(&self) -> ScrollView {
        ScrollView::new()
            .policy(self.policy.scroll_view_policy())
            .scrollbar_styles(self.styles.scrollbar)
    }

    /// Measurable rich text projected from caller-owned lines with highlight,
    /// selection, and cursor styles applied.
    #[must_use]
    pub fn text_block(&self) -> TextBlock {
        TextBlock::new(Text::from_lines(apply_ranges(
            self.lines,
            self.highlights,
            self.selection,
            self.cursor,
        )))
        .id(self.content_id())
        .style(self.styles.text)
        .alignment(self.policy.alignment)
        .wrap(self.policy.wrap)
        .trim(self.policy.trim)
    }

    /// Register selectable grapheme geometry for the visible text projection.
    ///
    /// Source offsets refer to the original caller-owned lines joined by
    /// newlines. Rows and graphemes clipped by the viewport are omitted by the
    /// scoped paint context.
    pub fn register_selection(
        &self,
        layout: &LayoutNode,
        selection: &ComponentSelectionState,
        policy: &ComponentSelectionPolicy,
        content_id: impl Into<bmux_tui::selection::SelectionContentId>,
        cx: &mut PaintCx<'_, '_>,
    ) -> ComponentSelectionOutcome {
        let outer = local_area_of(layout.size);
        let content_area = self.scroll_view().content_area(outer);
        let scope_outcome = paint_component_scope(cx, selection, policy, outer, content_area);
        if !policy.enabled || content_area.is_empty() || self.lines.is_empty() {
            return scope_outcome;
        }
        let Some((viewport, content)) = resolved_viewport(layout) else {
            return scope_outcome;
        };
        let state = self.state.get();
        let before = cx.selection().fragments().len();
        let content_id = content_id.into();
        cx.with_child(
            i32::from(content_area.x),
            i64::from(content_area.y),
            LocalRect::new(0, 0, content_area.width, content_area.height),
            |cx| {
                let x = state.horizontal_offset();
                let y = state.vertical_offset();
                cx.with_child(
                    -i32::try_from(x).unwrap_or(i32::MAX),
                    -i64::try_from(y).unwrap_or(i64::MAX),
                    LocalRect::new(
                        i32::try_from(x).unwrap_or(i32::MAX),
                        i64::try_from(y).unwrap_or(i64::MAX),
                        viewport.size.width,
                        u16::try_from(viewport.size.height).unwrap_or(u16::MAX),
                    ),
                    |cx| {
                        self.text_block().register_selection(
                            &content.node,
                            cx,
                            selection.scope_id.clone(),
                            content_id,
                            selection.order,
                            selection.revision,
                        );
                    },
                );
            },
        );
        let fragments = cx.selection().fragments().len().saturating_sub(before);
        if fragments == 0 {
            scope_outcome
        } else {
            ComponentSelectionOutcome::ContentRegistered { fragments }
        }
    }

    /// Handle one event against this component's authoritative layout.
    ///
    /// `area` is the terminal rectangle the component was painted into. The
    /// returned outcome describes shared scroll-state changes.
    #[must_use]
    pub fn handle_event(
        &self,
        area: Rect,
        layout: &LayoutNode,
        event: &Event,
    ) -> ScrollViewOutcome {
        let Some((viewport, _)) = resolved_viewport(layout) else {
            return ScrollViewOutcome::Ignored;
        };
        let scroll_view = self.scroll_view();
        let mut state = self.state.get();
        let mut outcome = scroll_view.handle_scrollbar_event(area, viewport, &mut state, event);
        if outcome == ScrollViewOutcome::Ignored && !state.dragging_scrollbar() {
            outcome = scroll_view.handle_event(
                scroll_view.content_area(area),
                viewport,
                &mut state,
                event,
            );
        }
        self.state.set(state);
        outcome
    }

    fn viewport_component(&self, viewport: LogicalSize) -> ScrollViewComponent<'_> {
        ScrollViewComponent::new(
            self.viewport_id(),
            viewport,
            self.state.get(),
            self.text_block(),
        )
    }

    fn natural_content_width(&self, content_width: u16) -> u16 {
        if self.policy.wrap == TextWrap::None {
            u16::try_from(self.lines.iter().map(Line::width).max().unwrap_or(0))
                .unwrap_or(u16::MAX)
                .max(content_width)
        } else {
            content_width
        }
    }
}

impl Component for TextViewComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let text = self.text_block().revision();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        text.layout.hash(&mut layout);
        self.policy.vertical_scrollbar.hash(&mut layout);
        self.policy.horizontal_scrollbar.hash(&mut layout);
        self.empty.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        text.paint.hash(&mut paint);
        self.policy.keyboard.hash(&mut paint);
        self.policy.mouse_wheel.hash(&mut paint);
        self.policy.background.hash(&mut paint);
        self.styles.empty.hash(&mut paint);
        self.styles.background.hash(&mut paint);
        self.styles.scrollbar.begin.hash(&mut paint);
        self.styles.scrollbar.track.hash(&mut paint);
        self.styles.scrollbar.thumb.hash(&mut paint);
        self.styles.scrollbar.end.hash(&mut paint);
        self.state.get().hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let scroll_view = self.scroll_view();
        let width = constraints.max_width();
        // Gutter reservation depends only on policy and outer width, so resolve
        // it against a tall probe rectangle before the height is known.
        let probe = scroll_view.content_area(Rect::new(0, 0, width, u16::MAX));
        let content_width = self.natural_content_width(probe.width);
        // Measure the content exactly once at its content width. Loose parents
        // size this view intrinsically from that height; tight parents clamp it.
        let content = self
            .text_block()
            .layout(Constraints::for_width(content_width), cx);
        let gutter_rows = usize::from(u16::MAX.saturating_sub(probe.height));
        let size = constraints.constrain(LogicalSize::new(
            width,
            content.size.height.max(1).saturating_add(gutter_rows),
        ));
        let content_area = scroll_view.content_area(local_area_of(size));
        let viewport = ScrollViewComponent::viewport_layout(
            self.viewport_id(),
            LogicalSize::new(content_area.width, usize::from(content_area.height)),
            content,
        );
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![ChildLayout::new(
                content_area.x,
                usize::from(content_area.y),
                viewport,
            )],
        )
        .with_metadata(LayoutMetadata::new().semantic("text-view"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let outer = local_area_of(layout.size);
        if outer.is_empty() {
            return;
        }
        if self.policy.background {
            cx.fill(
                LocalRect::new(0, 0, outer.width, outer.height),
                " ",
                self.styles.background,
            );
        }
        if self.lines.is_empty() {
            cx.write_line_with_fallback_style(
                LocalRect::new(0, 0, outer.width, 1),
                &Line::from(self.empty),
                self.styles.empty,
            );
            cx.push_damage(LocalRect::new(0, 0, outer.width, outer.height));
            return;
        }
        let Some((viewport, _)) = resolved_viewport(layout) else {
            return;
        };
        let scroll_view = self.scroll_view();
        let content_area = scroll_view.content_area(outer);
        let state = self.state.get();
        cx.with_child(
            i32::from(content_area.x),
            i64::from(content_area.y),
            LocalRect::new(0, 0, content_area.width, content_area.height),
            |cx| self.viewport_component(viewport.size).paint(viewport, cx),
        );
        let scrollable = ScrollView::max_vertical_offset(viewport) > 0
            || ScrollView::max_horizontal_offset(viewport) > 0;
        let has_gutter = self.policy.vertical_scrollbar != ScrollbarAxisLayoutMode::Hidden
            || self.policy.horizontal_scrollbar != ScrollbarAxisLayoutMode::Hidden;
        if has_gutter || (scrollable && (self.policy.keyboard || self.policy.mouse_wheel)) {
            scroll_view.paint_chrome(self.id.as_str(), outer, viewport, &state, cx);
        }
        cx.push_damage(LocalRect::new(0, 0, outer.width, outer.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        match self.handle_event(area, layout, event) {
            ScrollViewOutcome::Ignored => EventOutcome::Ignored,
            ScrollViewOutcome::Scrolled { .. } | ScrollViewOutcome::HorizontalScrolled { .. } => {
                EventOutcome::Redraw
            }
        }
    }
}

/// Resolve the inner scroll viewport and measured content nodes.
fn resolved_viewport(layout: &LayoutNode) -> Option<(&LayoutNode, &ChildLayout)> {
    let viewport = layout.children.first()?;
    let content = viewport.node.children.first()?;
    Some((&viewport.node, content))
}

fn apply_ranges(
    lines: &[Line],
    highlights: &[TextViewHighlight],
    selection: Option<TextViewSelection>,
    cursor: Option<TextViewCursor>,
) -> Vec<Line> {
    if highlights.is_empty() && selection.is_none() && cursor.is_none() {
        return lines.to_vec();
    }
    lines
        .iter()
        .enumerate()
        .map(|(line_index, line)| {
            apply_line_ranges(line, line_index, highlights, selection, cursor)
        })
        .collect()
}

fn apply_line_ranges(
    line: &Line,
    line_index: usize,
    highlights: &[TextViewHighlight],
    selection: Option<TextViewSelection>,
    cursor: Option<TextViewCursor>,
) -> Line {
    let line_highlights = highlights
        .iter()
        .copied()
        .filter(|highlight| highlight.line == line_index && highlight.start < highlight.end)
        .collect::<Vec<_>>();
    let line_selection = selection
        .filter(|selection| selection.line == line_index && selection.start < selection.end);
    let line_cursor = cursor.filter(|cursor| cursor.line == line_index);
    if line_highlights.is_empty() && line_selection.is_none() && line_cursor.is_none() {
        return line.clone();
    }
    let mut spans = Vec::new();
    let mut char_index = 0usize;
    for span in &line.spans {
        for ch in span.content.chars() {
            let mut style = line_highlights
                .iter()
                .filter(|highlight| char_index >= highlight.start && char_index < highlight.end)
                .fold(span.style, |style, highlight| style.patch(highlight.style));
            if let Some(selection) = line_selection
                && char_index >= selection.start
                && char_index < selection.end
            {
                style = style.patch(selection.style);
            }
            if let Some(cursor) = line_cursor
                && char_index == cursor.column
            {
                style = style.patch(cursor.style);
            }
            spans.push(Span::styled(ch.to_string(), style));
            char_index = char_index.saturating_add(1);
        }
    }
    Line::from_spans(spans)
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`TextViewStyles`].
    #[must_use]
    pub fn text_view_styles(self) -> TextViewStyles {
        TextViewStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for TextViewStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let scrollbar = theme.scrollbar_styles();
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            text: theme.text,
            empty: theme.muted,
            background: theme.surfaces.normal,
            scrollbar,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutNode};
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::prelude::{Alignment, Line, TextWrap};
    use bmux_tui::style::{Color, Style};

    use super::{
        TextViewComponent, TextViewCursor, TextViewHighlight, TextViewPolicy, TextViewSelection,
    };
    use crate::scroll_view::{ScrollViewOutcome, ScrollViewState};
    use crate::scrollbar_layout::ScrollbarAxisLayoutMode;
    use crate::selection::{
        ComponentSelectionOutcome, ComponentSelectionPolicy, ComponentSelectionState,
    };

    fn layout_at(component: &impl Component, area: Rect) -> LayoutNode {
        component.layout(Constraints::tight(area.size()), &mut LayoutCx::new())
    }

    fn paint_at(component: &impl Component, area: Rect, frame: &mut Frame<'_>) -> LayoutNode {
        let layout = layout_at(component, area);
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
        layout
    }

    fn rows(frame: &Frame<'_>, area: Rect) -> Vec<String> {
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .filter_map(|x| frame.buffer().get(Point::new(x, y)))
                    .map(|cell| cell.symbol.as_str())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn measures_exact_wrapped_height_for_loose_parents() {
        let lines = [Line::from("abcdef")];
        let state = Cell::new(ScrollViewState::new());
        let view = TextViewComponent::new("text", &lines, &state).policy(TextViewPolicy {
            wrap: TextWrap::Character,
            ..TextViewPolicy::bare()
        });

        let layout = view.layout(Constraints::for_width(3), &mut LayoutCx::new());

        assert_eq!(layout.size.height, 2);
        assert_eq!(layout.children[0].node.children[0].node.size.height, 2);
    }

    #[test]
    fn paints_registers_exact_scrollable_viewport_and_scrollbar_geometry() {
        let lines = [
            Line::from("zero"),
            Line::from("one"),
            Line::from("two"),
            Line::from("three"),
        ];
        let state = Cell::new(ScrollViewState::new());
        let view = TextViewComponent::new("preview", &lines, &state).policy(
            TextViewPolicy::scrollable().vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter),
        );
        let mut buffer = Buffer::empty(Rect::new(3, 2, 14, 5));
        let mut frame = Frame::new(&mut buffer);

        let layout = paint_at(&view, Rect::new(6, 3, 10, 3), &mut frame);

        assert_eq!(layout.size.height, 3);
        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].id.as_str(), "preview");
        assert_eq!(regions[0].area, Rect::new(6, 3, 9, 3));
        assert_eq!(regions[0].role, HitRole::Scroll);
        assert!(regions[0].focusable);
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].role, "scroll");
        assert_eq!(
            rows(&frame, Rect::new(6, 3, 10, 3)),
            vec!["zero     █", "one      █", "two      │"]
        );
    }

    #[test]
    fn non_scrollable_bare_and_empty_views_register_nothing() {
        let short = [Line::from("one")];
        let long = [Line::from("zero"), Line::from("one")];
        let empty: [Line; 0] = [];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 4));
        let mut frame = Frame::new(&mut buffer);
        let state = Cell::new(ScrollViewState::new());

        paint_at(
            &TextViewComponent::new("short", &short, &state),
            Rect::new(0, 0, 10, 2),
            &mut frame,
        );
        paint_at(
            &TextViewComponent::new("bare", &long, &state).policy(TextViewPolicy::bare()),
            Rect::new(0, 2, 10, 1),
            &mut frame,
        );
        paint_at(
            &TextViewComponent::new("empty", &empty, &state).empty("Nothing here"),
            Rect::new(0, 3, 10, 1),
            &mut frame,
        );

        assert!(frame.hits().regions().is_empty());
        assert_eq!(frame.buffer().row_symbols(3).as_deref(), Some("Nothing he"));
    }

    #[test]
    fn golden_both_axis_gutters_share_the_scroll_view_layout() {
        let lines = [
            Line::from("abcdef"),
            Line::from("ghijkl"),
            Line::from("mnopqr"),
        ];
        let mut state = ScrollViewState::new();
        state.set_vertical_offset(1);
        state.set_horizontal_offset(1);
        let state = Cell::new(state);
        let view = TextViewComponent::new("text", &lines, &state).policy(
            TextViewPolicy::bare()
                .vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter)
                .horizontal_scrollbar(ScrollbarAxisLayoutMode::Gutter),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
        let mut frame = Frame::new(&mut buffer);

        let layout = paint_at(&view, Rect::new(0, 0, 4, 3), &mut frame);

        assert_eq!(layout.children[0].node.size.width, 3);
        assert_eq!(layout.children[0].node.size.height, 2);
        assert_eq!(
            rows(&frame, Rect::new(0, 0, 4, 3)),
            vec!["hij│", "nop█", "█── ",]
        );
    }

    #[test]
    fn horizontal_offset_clips_no_wrap_content_and_wide_graphemes() {
        let lines = [Line::from("abcdef")];
        let mut state = ScrollViewState::new();
        state.set_horizontal_offset(2);
        let state = Cell::new(state);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        paint_at(
            &TextViewComponent::new("text", &lines, &state).policy(TextViewPolicy::bare()),
            Rect::new(0, 0, 3, 1),
            &mut frame,
        );
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("cde"));

        let wide = [Line::from("a界b")];
        let mut wide_state = ScrollViewState::new();
        wide_state.set_horizontal_offset(2);
        let wide_state = Cell::new(wide_state);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let mut frame = Frame::new(&mut buffer);
        paint_at(
            &TextViewComponent::new("wide", &wide, &wide_state).policy(TextViewPolicy::bare()),
            Rect::new(0, 0, 2, 1),
            &mut frame,
        );
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("b "));

        wide_state.set(ScrollViewState::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 2));
        let mut frame = Frame::new(&mut buffer);
        paint_at(
            &TextViewComponent::new("wrap", &wide, &wide_state).policy(TextViewPolicy {
                wrap: TextWrap::Character,
                ..TextViewPolicy::bare()
            }),
            Rect::new(0, 0, 3, 2),
            &mut frame,
        );
        assert_eq!(rows(&frame, Rect::new(0, 0, 3, 2)), vec!["a界", "b  "]);
    }

    #[test]
    fn highlights_selection_and_cursor_patch_styles_without_owning_state() {
        let lines = [Line::from("abcdef")];
        let highlights = [TextViewHighlight::new(
            0,
            0,
            2,
            Style::new().fg(Color::Yellow),
        )];
        let state = Cell::new(ScrollViewState::new());
        let view = TextViewComponent::new("text", &lines, &state)
            .policy(TextViewPolicy::bare())
            .highlights(&highlights)
            .selection(Some(TextViewSelection::new(
                0,
                1,
                3,
                Style::new().bg(Color::Blue),
            )))
            .cursor(Some(TextViewCursor::new(0, 4, Style::new().fg(Color::Red))));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        paint_at(&view, Rect::new(0, 0, 6, 1), &mut frame);

        let style = |x: u16| frame.buffer().get(Point::new(x, 0)).map(|cell| cell.style);
        assert_eq!(style(0), Some(Style::new().fg(Color::Yellow)));
        assert_eq!(
            style(1),
            Some(Style::new().fg(Color::Yellow).bg(Color::Blue))
        );
        assert_eq!(style(2), Some(Style::new().bg(Color::Blue)));
        assert_eq!(style(4), Some(Style::new().fg(Color::Red)));
    }

    #[test]
    fn word_wrap_trim_and_center_alignment_use_measured_text_block() {
        let state = Cell::new(ScrollViewState::new());
        let wrapped = [Line::from("one two")];
        let view = TextViewComponent::new("wrap", &wrapped, &state).policy(TextViewPolicy {
            wrap: TextWrap::Word,
            trim: true,
            ..TextViewPolicy::bare()
        });
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);
        paint_at(&view, Rect::new(0, 0, 6, 2), &mut frame);
        assert_eq!(
            rows(&frame, Rect::new(0, 0, 6, 2)),
            vec!["one   ", "two   "]
        );

        let centered = [Line::from("hi")];
        let view = TextViewComponent::new("center", &centered, &state).policy(TextViewPolicy {
            alignment: Alignment::Center,
            ..TextViewPolicy::bare()
        });
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);
        paint_at(&view, Rect::new(0, 0, 6, 1), &mut frame);
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("  hi  "));
    }

    #[test]
    fn keyboard_and_wheel_scroll_through_resolved_layout_and_clamp() {
        let lines = [Line::from("one"), Line::from("two"), Line::from("three")];
        let mut initial = ScrollViewState::new();
        initial.interaction.focused = true;
        let state = Cell::new(initial);
        let view = TextViewComponent::new("text", &lines, &state);
        let area = Rect::new(2, 1, 10, 1);
        let layout = layout_at(&view, area);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 4));
        let mut frame = Frame::new(&mut buffer);
        paint_at(&view, area, &mut frame);
        let hit = &frame.hits().regions()[0];
        let mut cx = EventCx::with_clip(&layout, hit.area);
        cx.with_transform(
            0,
            0,
            i32::from(hit.area.x),
            i64::from(hit.area.y),
            hit.area,
            |cx| {
                assert_eq!(
                    view.event(&Event::Key(KeyStroke::simple(KeyCode::Down)), &layout, cx),
                    EventOutcome::Redraw
                );
            },
        );
        assert_eq!(state.get().vertical_offset(), 1);

        assert_eq!(
            view.handle_event(
                area,
                &layout,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::ScrollDown,
                    Point::new(3, 1)
                )),
            ),
            ScrollViewOutcome::Scrolled { vertical_offset: 2 }
        );
        assert_eq!(
            view.handle_event(
                area,
                &layout,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::ScrollDown,
                    Point::new(3, 1)
                )),
            ),
            ScrollViewOutcome::Ignored
        );
        assert!(state.get().follows_bottom());
        assert_eq!(
            view.handle_event(
                area,
                &layout,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::ScrollDown,
                    Point::new(13, 3)
                )),
            ),
            ScrollViewOutcome::Ignored
        );
    }

    #[test]
    fn keyboard_left_right_scroll_horizontally_only_in_no_wrap_mode() {
        let lines = [Line::from("abcdef")];
        let mut initial = ScrollViewState::new();
        initial.interaction.focused = true;
        let state = Cell::new(initial);
        let area = Rect::new(0, 0, 3, 1);
        let view = TextViewComponent::new("text", &lines, &state).policy(TextViewPolicy {
            keyboard: true,
            ..TextViewPolicy::bare()
        });
        let layout = layout_at(&view, area);

        assert_eq!(
            view.handle_event(
                area,
                &layout,
                &Event::Key(KeyStroke::simple(KeyCode::Right))
            ),
            ScrollViewOutcome::HorizontalScrolled {
                horizontal_offset: 1
            }
        );
        state.set({
            let mut next = state.get();
            next.set_horizontal_offset(99);
            next
        });
        assert_eq!(
            view.handle_event(
                area,
                &layout,
                &Event::Key(KeyStroke::simple(KeyCode::Right))
            ),
            ScrollViewOutcome::HorizontalScrolled {
                horizontal_offset: 3
            }
        );

        let wrapped = TextViewComponent::new("wrap", &lines, &state);
        let wrapped_layout = layout_at(&wrapped, area);
        state.set({
            let mut next = ScrollViewState::new();
            next.interaction.focused = true;
            next
        });
        assert_eq!(
            wrapped.handle_event(
                area,
                &wrapped_layout,
                &Event::Key(KeyStroke::simple(KeyCode::Right))
            ),
            ScrollViewOutcome::Ignored
        );
        assert_eq!(
            wrapped_layout.children[0].node.children[0].node.size.width,
            3
        );
    }

    #[test]
    fn scrollbar_drag_routes_into_shared_scroll_state() {
        let lines: Vec<Line> = (0..20)
            .map(|index| Line::from(format!("{index}")))
            .collect();
        let state = Cell::new(ScrollViewState::new());
        let view = TextViewComponent::new("text", &lines, &state).policy(
            TextViewPolicy::scrollable().vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter),
        );
        let area = Rect::new(0, 0, 6, 5);
        let layout = layout_at(&view, area);

        assert!(matches!(
            view.handle_event(
                area,
                &layout,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(5, 4)
                )),
            ),
            ScrollViewOutcome::Scrolled { .. }
        ));
        assert!(state.get().dragging_scrollbar());
        assert_eq!(state.get().vertical_offset(), 15);
        assert_eq!(
            view.handle_event(
                area,
                &layout,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(5, 4)
                )),
            ),
            ScrollViewOutcome::Ignored
        );
        assert!(!state.get().dragging_scrollbar());
    }

    #[test]
    fn selection_fragments_map_visible_graphemes_to_source_offsets() {
        let lines = [Line::from("a界e\u{301}z"), Line::from("second")];
        let mut initial = ScrollViewState::new();
        initial.set_vertical_offset(1);
        initial.set_horizontal_offset(3);
        let state = Cell::new(initial);
        let view = TextViewComponent::new("text", &lines, &state).policy(TextViewPolicy::bare());
        let area = Rect::new(0, 0, 3, 1);
        let layout = layout_at(&view, area);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let selection = ComponentSelectionState::new("text");

        PaintCx::new(&mut frame).with_child(0, 0, LocalRect::new(0, 0, 3, 1), |cx| {
            assert_eq!(
                view.register_selection(
                    &layout,
                    &selection,
                    &ComponentSelectionPolicy::content(),
                    "document",
                    cx,
                ),
                ComponentSelectionOutcome::ContentRegistered { fragments: 3 }
            );
        });

        let fragments = frame.selection().fragments();
        assert_eq!(fragments.len(), 3);
        // "a界e\u{301}z\n" occupies 9 bytes; the second line starts at 9, and
        // a horizontal offset of 3 skips "sec".
        assert!(
            fragments
                .iter()
                .all(|fragment| fragment.source_range.start >= 12)
        );
        assert!(fragments.iter().all(|fragment| fragment.area.right() <= 3));
        assert_eq!(frame.selection().scopes().len(), 1);
    }

    #[test]
    fn revision_separates_scroll_and_style_from_content_changes() {
        let lines = [Line::from("one"), Line::from("two")];
        let state = Cell::new(ScrollViewState::new());
        let view = TextViewComponent::new("text", &lines, &state);
        let base = view.revision();

        state.set({
            let mut next = ScrollViewState::new();
            next.set_vertical_offset(1);
            next
        });
        let scrolled = view.revision();
        assert_eq!(base.layout, scrolled.layout);
        assert_ne!(base.paint, scrolled.paint);

        let wrapped = TextViewComponent::new("text", &lines, &state).policy(TextViewPolicy {
            wrap: TextWrap::None,
            ..TextViewPolicy::scrollable()
        });
        assert_ne!(scrolled.layout, wrapped.revision().layout);
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let lines = [Line::from("hello")];
        let state = Cell::new(ScrollViewState::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        paint_at(
            &TextViewComponent::new("text", &lines, &state),
            Rect::new(0, 0, 0, 0),
            &mut frame,
        );
    }
}
