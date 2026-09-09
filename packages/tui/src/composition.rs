//! Fundamental measurable composition containers.

use std::hash::{Hash, Hasher};
use std::ops::Range;

use crate::chrome::Border;
use crate::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, LayoutCx, LayoutId,
    LayoutNode, LogicalSize, combine_child_revisions,
};
use crate::geometry::{Insets, Rect};
use crate::paint::{LocalRect, PaintCx};
use crate::selection::{SelectionContentId, SelectionScopeId, plain_text_fragments};
use crate::style::Style;
use crate::text::{Line, Text, TextWrap, TextWrapGeometry};
use crate::text_block::Alignment;

fn stable_revision(hash: impl FnOnce(&mut std::collections::hash_map::DefaultHasher)) -> u64 {
    let mut state = std::collections::hash_map::DefaultHasher::new();
    hash(&mut state);
    state.finish()
}

fn event_single_child(
    child: &Element<'_>,
    event: &crate::event::Event,
    layout: &LayoutNode,
    cx: &mut crate::component::EventCx<'_>,
) -> crate::event::EventOutcome {
    layout
        .children
        .first()
        .map_or(crate::event::EventOutcome::Ignored, |resolved| {
            cx.with_child(resolved, |cx| child.event(event, &resolved.node, cx))
        })
}

fn event_children(
    children: &[Element<'_>],
    event: &crate::event::Event,
    layout: &LayoutNode,
    cx: &mut crate::component::EventCx<'_>,
) -> crate::event::EventOutcome {
    children
        .iter()
        .rev()
        .zip(layout.children.iter().rev())
        .find_map(|(child, resolved)| {
            let outcome = cx.with_child(resolved, |cx| child.event(event, &resolved.node, cx));
            outcome.is_handled().then_some(outcome)
        })
        .unwrap_or(crate::event::EventOutcome::Ignored)
}

fn hash_insets(insets: Insets, state: &mut impl Hasher) {
    insets.top.hash(state);
    insets.right.hash(state);
    insets.bottom.hash(state);
    insets.left.hash(state);
}

fn hash_border(border: Option<&Border>, state: &mut impl Hasher) {
    let Some(border) = border else {
        0u8.hash(state);
        return;
    };
    1u8.hash(state);
    border.set.top_left.hash(state);
    border.set.top_right.hash(state);
    border.set.bottom_left.hash(state);
    border.set.bottom_right.hash(state);
    border.set.horizontal.hash(state);
    border.set.vertical.hash(state);
    border.style.hash(state);
    border.sides.top.hash(state);
    border.sides.right.hash(state);
    border.sides.bottom.hash(state);
    border.sides.left.hash(state);
}

/// Horizontal placement inside an assigned component rectangle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HorizontalAlignment {
    /// Place the child at the leading edge.
    #[default]
    Start,
    /// Center the child.
    Center,
    /// Place the child at the trailing edge.
    End,
    /// Expand the child to the assigned width.
    Stretch,
}

/// Vertical placement inside an assigned component rectangle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VerticalAlignment {
    /// Place the child at the top edge.
    #[default]
    Start,
    /// Center the child.
    Center,
    /// Place the child at the bottom edge.
    End,
    /// Expand the child to the assigned height.
    Stretch,
}

/// One rendered rich-text row and its UTF-8 byte range in the logical source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextProjectionRow {
    /// Styled rendered row.
    pub line: Line,
    /// Zero-based source line index.
    pub source_line: usize,
    /// UTF-8 byte range in the newline-separated logical text.
    pub source_range: Range<usize>,
}

/// A measurable rich-text component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
    id: LayoutId,
    text: Text,
    /// Byte offsets into the immutable newline-separated source text.
    source_line_offsets: Vec<usize>,
    text_revision: ComponentRevision,
    intrinsic_width: usize,
    style: Style,
    alignment: Alignment,
    wrap: TextWrap,
    trim: bool,
    vertical_scroll: usize,
}

impl TextBlock {
    /// Create rich text content.
    #[must_use]
    pub fn new(text: impl Into<Text>) -> Self {
        let text = text.into();
        let intrinsic_width = text.width();
        let mut offset = 0usize;
        let source_line_offsets = text
            .lines
            .iter()
            .map(|line| {
                let start = offset;
                offset = line
                    .spans
                    .iter()
                    .fold(offset, |base, span| base.saturating_add(span.content.len()));
                offset = offset.saturating_add(1);
                start
            })
            .collect();
        let text_revision = ComponentRevision::new(
            stable_revision(|state| {
                text.lines.len().hash(state);
                for line in &text.lines {
                    line.spans.len().hash(state);
                    for span in &line.spans {
                        span.content.hash(state);
                    }
                }
            }),
            stable_revision(|state| {
                for line in &text.lines {
                    for span in &line.spans {
                        span.style.hash(state);
                    }
                }
            }),
        );
        Self {
            id: LayoutId::new("text"),
            text,
            source_line_offsets,
            text_revision,
            intrinsic_width,
            style: Style::new(),
            alignment: Alignment::Left,
            wrap: TextWrap::Word,
            trim: false,
            vertical_scroll: 0,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set base text and row style.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set text wrapping.
    #[must_use]
    pub const fn wrap(mut self, wrap: TextWrap) -> Self {
        self.wrap = wrap;
        self
    }

    /// Set horizontal alignment.
    #[must_use]
    pub const fn alignment(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Set whether trailing whitespace is removed from each projected row.
    #[must_use]
    pub const fn trim(mut self, trim: bool) -> Self {
        self.trim = trim;
        self
    }

    /// Skip logical projected rows before painting.
    #[must_use]
    pub const fn vertical_scroll(mut self, vertical_scroll: usize) -> Self {
        self.vertical_scroll = vertical_scroll;
        self
    }

    /// Return this component's canonical rich text.
    #[must_use]
    pub const fn text(&self) -> &Text {
        &self.text
    }

    fn own_revision(&self) -> ComponentRevision {
        let layout = stable_revision(|state| {
            self.id.as_str().hash(state);
            self.text_revision.layout.hash(state);
            (match self.wrap {
                TextWrap::None => 0u8,
                TextWrap::Character => 1,
                TextWrap::Word => 2,
            })
            .hash(state);
            self.trim.hash(state);
        });
        let paint = stable_revision(|state| {
            self.style.hash(state);
            self.text_revision.paint.hash(state);
            (match self.alignment {
                Alignment::Left => 0u8,
                Alignment::Center => 1,
                Alignment::Right => 2,
            })
            .hash(state);
            self.vertical_scroll.hash(state);
        });
        ComponentRevision::new(layout, paint)
    }

    /// Project measured rows back to UTF-8 byte ranges in the logical text.
    ///
    /// Pass this component's resolved layout, with a matching layout revision.
    /// Paint-only style changes may reuse it. Source lines are newline-separated;
    /// ranges exclude separators and whitespace consumed by word wrapping.
    /// This returns the complete measured content, before viewport scrolling.
    #[must_use]
    pub fn projection(&self, layout: &LayoutNode) -> Vec<TextProjectionRow> {
        self.measured_rows(layout, 0).collect()
    }

    fn raw_line_rows<'a>(&'a self, line: &'a Line, width: u16) -> impl Iterator<Item = Line> + 'a {
        let geometry = TextWrapGeometry::uniform(usize::from(width.max(1)));
        let mut character = crate::text::character_rows(line, geometry);
        let mut word =
            (self.wrap == TextWrap::Word).then(|| crate::text::word_rows(line, geometry));
        let mut unwrapped = Some(line);
        std::iter::from_fn(move || match self.wrap {
            TextWrap::Character => character.next(),
            TextWrap::Word => word.as_mut().and_then(Iterator::next),
            TextWrap::None => unwrapped.take().cloned(),
        })
    }

    fn projection_rows(&self, width: u16) -> impl Iterator<Item = TextProjectionRow> + '_ {
        self.projection_rows_from(width, 0)
    }

    fn projection_rows_from(
        &self,
        width: u16,
        first_line: usize,
    ) -> impl Iterator<Item = TextProjectionRow> + '_ {
        self.text
            .lines
            .iter()
            .enumerate()
            .skip(first_line)
            .flat_map(move |(source_line, line)| {
                let mut source = line.spans.iter().flat_map(|span| span.content.bytes());
                let line_base = self.source_line_offsets[source_line];
                let mut search_start = 0usize;
                self.raw_line_rows(line, width).map(move |raw| {
                    let raw_len = raw
                        .spans
                        .iter()
                        .map(|span| span.content.len())
                        .sum::<usize>();
                    let rendered = if self.trim { trim_line_end(raw) } else { raw };
                    let rendered_len = rendered
                        .spans
                        .iter()
                        .map(|span| span.content.len())
                        .sum::<usize>();
                    let mut relative = search_start;
                    if self.wrap == TextWrap::Word && rendered_len > 0 {
                        relative = relative.saturating_add(advance_to_match(
                            &mut source,
                            rendered.spans.iter().flat_map(|span| span.content.bytes()),
                        ));
                    }
                    let start = line_base.saturating_add(relative);
                    if self.wrap == TextWrap::Word {
                        search_start = relative.saturating_add(rendered_len);
                    } else {
                        search_start = search_start.saturating_add(raw_len);
                    }
                    TextProjectionRow {
                        line: rendered,
                        source_line,
                        source_range: start..start.saturating_add(rendered_len),
                    }
                })
            })
    }

    /// Register selectable grapheme geometry from the authoritative projection.
    ///
    /// The caller owns scope/content identity and revision. Local rows are
    /// translated and clipped by [`PaintCx`], and partially clipped graphemes
    /// are omitted by its standard selection registration path.
    pub fn register_selection(
        &self,
        layout: &LayoutNode,
        cx: &mut PaintCx<'_, '_>,
        scope_id: impl Into<SelectionScopeId>,
        content_id: impl Into<SelectionContentId>,
        order: u64,
        revision: u64,
    ) {
        let scope_id = scope_id.into();
        let content_id = content_id.into();
        let visible = Self::visible_rows(layout, cx.area());
        if visible.is_empty() {
            return;
        }
        let first = self.vertical_scroll.saturating_add(visible.start);
        for (index, row) in self
            .measured_rows(layout, first)
            .take(visible.len())
            .enumerate()
        {
            let row_index = visible.start.saturating_add(index);
            let text = row.line.plain_text();
            let x = self.row_alignment_offset(&row.line, layout.size.width);
            let y = i64::try_from(row_index).unwrap_or(i64::MAX);
            cx.with_child(0, y, LocalRect::new(0, 0, layout.size.width, 1), |cx| {
                for fragment in plain_text_fragments(
                    scope_id.clone(),
                    content_id.clone(),
                    Rect::new(x, 0, layout.size.width.saturating_sub(x), 1),
                    order.saturating_add(u64::try_from(row_index).unwrap_or(u64::MAX)),
                    &text,
                    row.source_range.start,
                    revision,
                ) {
                    cx.push_selection_fragment(fragment);
                }
            });
        }
    }

    fn measured_rows<'a>(
        &'a self,
        layout: &'a LayoutNode,
        first: usize,
    ) -> impl Iterator<Item = TextProjectionRow> + 'a {
        layout
            .metadata
            .text_rows
            .iter()
            .flat_map(|rows| rows.iter())
            .skip(first)
            .map(|row| {
                let base = self.source_line_offsets[row.source_line];
                let range = row.source_range.start.saturating_sub(base)
                    ..row.source_range.end.saturating_sub(base);
                let mut offset = 0usize;
                let spans = self.text.lines[row.source_line]
                    .spans
                    .iter()
                    .filter_map(|span| {
                        let start = range.start.saturating_sub(offset).min(span.content.len());
                        let end = range.end.saturating_sub(offset).min(span.content.len());
                        offset = offset.saturating_add(span.content.len());
                        (start < end).then(|| {
                            crate::text::Span::styled(&span.content[start..end], span.style)
                        })
                    })
                    .collect::<Vec<_>>();
                TextProjectionRow {
                    source_line: row.source_line,
                    source_range: row.source_range.clone(),
                    line: Line::from_spans(spans),
                }
            })
    }

    fn row_alignment_offset(&self, line: &Line, width: u16) -> u16 {
        let line_width = u16::try_from(line.width()).unwrap_or(u16::MAX);
        let remaining = width.saturating_sub(line_width);
        match self.alignment {
            Alignment::Left => 0,
            Alignment::Center => remaining / 2,
            Alignment::Right => remaining,
        }
    }

    fn visible_rows(layout: &LayoutNode, visible: LocalRect) -> std::ops::Range<usize> {
        if visible.width == 0
            || visible.height == 0
            || visible.x >= i32::from(layout.size.width)
            || visible.x.saturating_add(i32::from(visible.width)) <= 0
        {
            return 0..0;
        }
        let first = usize::try_from(visible.y.max(0))
            .unwrap_or(usize::MAX)
            .min(layout.size.height);
        let end = usize::try_from(visible.y.saturating_add(i64::from(visible.height)).max(0))
            .unwrap_or(usize::MAX)
            .min(layout.size.height);
        first..end
    }

    #[cfg(test)]
    fn line_row_count(&self, line: &Line, width: u16) -> usize {
        let geometry = TextWrapGeometry::uniform(usize::from(width.max(1)));
        match self.wrap {
            TextWrap::Character => crate::text::character_row_count(line, geometry),
            TextWrap::Word => crate::text::word_row_count(line, geometry),
            TextWrap::None => 1,
        }
    }

    /// Locate a display row without constructing preceding rendered lines.
    /// The returned offset is relative to the first retained source line.
    #[cfg(test)]
    fn source_start(&self, width: u16, mut row: usize) -> (usize, usize) {
        if self.wrap == TextWrap::None {
            return (row.min(self.text.lines.len()), 0);
        }
        for (index, line) in self.text.lines.iter().enumerate() {
            // Every source line produces at least one row, including empty lines.
            // At a line boundary no measurement of the retained line is needed.
            if row == 0 {
                return (index, 0);
            }
            let geometry = TextWrapGeometry::uniform(usize::from(width.max(1)));
            let limit = row.saturating_add(1);
            let count = match self.wrap {
                TextWrap::Character => {
                    crate::text::character_row_count_up_to(line, geometry, limit)
                }
                TextWrap::Word => crate::text::word_row_count_up_to(line, geometry, limit),
                TextWrap::None => 1,
            };
            if row < count {
                return (index, row);
            }
            row = row.saturating_sub(count);
        }
        (self.text.lines.len(), 0)
    }

    #[cfg(test)]
    fn row_count(&self, width: u16) -> usize {
        if self.wrap == TextWrap::None {
            return self.text.lines.len();
        }
        self.text.lines.iter().fold(0usize, |count, line| {
            count.saturating_add(self.line_row_count(line, width))
        })
    }
}

/// Advance past a matching row, returning the number of skipped source bytes.
fn advance_to_match(
    source: &mut (impl Iterator<Item = u8> + Clone),
    mut needle: impl Iterator<Item = u8> + Clone,
) -> usize {
    let Some(first) = needle.next() else {
        return 0;
    };
    let mut candidate = source.clone();
    let mut skipped = 0usize;
    while let Some(byte) = candidate.next() {
        if byte == first {
            let mut comparison = candidate.clone();
            if needle
                .clone()
                .all(|expected| comparison.next() == Some(expected))
            {
                *source = comparison;
                return skipped;
            }
        }
        skipped = skipped.saturating_add(1);
    }
    // Preserve the existing fallback if a projected row cannot be located.
    for _ in std::iter::once(first).chain(needle) {
        if source.next().is_none() {
            break;
        }
    }
    0
}

fn trim_line_end(mut line: Line) -> Line {
    while let Some(last) = line.spans.last_mut() {
        let trimmed_len = last.content.trim_end().len();
        if trimmed_len == last.content.len() {
            break;
        }
        last.content.truncate(trimmed_len);
        if !last.content.is_empty() {
            break;
        }
        line.spans.pop();
    }
    line
}

impl Component for TextBlock {
    fn revision(&self) -> ComponentRevision {
        self.own_revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16::try_from(self.intrinsic_width)
                .unwrap_or(u16::MAX)
                .clamp(constraints.min_width(), constraints.max_width())
        };
        let rows = self
            .projection_rows(width)
            .map(|row| crate::component::TextRowGeometry {
                source_line: row.source_line,
                source_range: row.source_range,
            })
            .collect::<Vec<_>>();
        let size = constraints.constrain(LogicalSize::new(width, rows.len()));
        let mut node = LayoutNode::leaf(self.id.clone(), size);
        node.metadata.text_rows = Some(rows.into());
        node
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let visible = Self::visible_rows(layout, cx.area());
        if visible.is_empty() {
            return;
        }
        let first = self.vertical_scroll.saturating_add(visible.start);
        for (index, mut line) in self
            .measured_rows(layout, first)
            .map(|row| row.line)
            .take(visible.len())
            .enumerate()
        {
            let x = self.row_alignment_offset(&line, layout.size.width);
            if x > 0 {
                line.spans
                    .insert(0, crate::text::Span::raw(" ".repeat(usize::from(x))));
            }
            let row = i64::try_from(visible.start.saturating_add(index)).unwrap_or(i64::MAX);
            cx.write_line_with_fallback_style(
                LocalRect::new(0, row, layout.size.width, 1),
                &line,
                self.style,
            );
        }
    }
}

/// A child-owning rectangular style, border, and padding container.
pub struct Surface<'a> {
    id: LayoutId,
    child: Element<'a>,
    background: Style,
    content_style: Style,
    border: Option<Border>,
    paint_border: bool,
    padding: Insets,
}

impl<'a> Surface<'a> {
    /// Create a surface containing one child.
    #[must_use]
    pub fn new(child: impl Component + 'a) -> Self {
        Self {
            id: LayoutId::new("surface"),
            child: Element::new(child),
            background: Style::new(),
            content_style: Style::new(),
            border: None,
            paint_border: true,
            padding: Insets::all(0),
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set complete rectangular background style.
    #[must_use]
    pub const fn background(mut self, style: Style) -> Self {
        self.background = style;
        self
    }

    /// Set inherited child content style.
    #[must_use]
    pub const fn content_style(mut self, style: Style) -> Self {
        self.content_style = style;
        self
    }

    /// Set border.
    #[must_use]
    pub const fn border(mut self, border: Border) -> Self {
        self.border = Some(border);
        self
    }

    /// Control whether the configured border is painted while retaining its layout insets.
    #[must_use]
    pub const fn paint_border(mut self, paint_border: bool) -> Self {
        self.paint_border = paint_border;
        self
    }

    /// Set child padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    const fn insets(&self) -> Insets {
        let border = match &self.border {
            Some(border) => border.sides.insets(),
            None => Insets::all(0),
        };
        Insets::new(
            border.top.saturating_add(self.padding.top),
            border.right.saturating_add(self.padding.right),
            border.bottom.saturating_add(self.padding.bottom),
            border.left.saturating_add(self.padding.left),
        )
    }

    fn own_revision(&self) -> ComponentRevision {
        let layout = stable_revision(|state| {
            self.id.as_str().hash(state);
            hash_insets(self.insets(), state);
        });
        let paint = stable_revision(|state| {
            self.background.hash(state);
            self.content_style.hash(state);
            self.paint_border.hash(state);
            hash_border(self.border.as_ref(), state);
        });
        ComponentRevision::new(layout, paint)
    }
}

impl Component for Surface<'_> {
    fn revision(&self) -> ComponentRevision {
        self.own_revision().combine(self.child.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let insets = self.insets();
        let child = self.child.layout(
            constraints.inset(insets.horizontal(), usize::from(insets.vertical())),
            cx,
        );
        let size = constraints.constrain(LogicalSize::new(
            child.size.width.saturating_add(insets.horizontal()),
            child
                .size
                .height
                .saturating_add(usize::from(insets.vertical())),
        ));
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![ChildLayout::new(
                insets.left,
                usize::from(insets.top),
                child,
            )],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        cx.with_child_size(0, 0, layout.size, |cx| {
            cx.fill(cx.area(), " ", self.background);
            if self.paint_border
                && let Some(border) = &self.border
            {
                paint_border(
                    layout.size.width,
                    layout.size.height,
                    border,
                    self.background,
                    cx,
                );
            }
            let Some(child_layout) = layout.children.first() else {
                return;
            };
            cx.with_style(self.content_style, |cx| {
                cx.with_child_size(
                    i32::from(child_layout.x),
                    i64::try_from(child_layout.y).unwrap_or(i64::MAX),
                    child_layout.node.size,
                    |cx| self.child.paint(&child_layout.node, cx),
                );
            });
        });
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        event_single_child(&self.child, event, layout, cx)
    }
}

fn paint_border(
    width: u16,
    height: usize,
    border: &Border,
    background: Style,
    cx: &mut PaintCx<'_, '_>,
) {
    if width == 0 || height == 0 {
        return;
    }
    let style = background.patch(border.style);
    let right = width.saturating_sub(1);
    let bottom = i64::try_from(height.saturating_sub(1)).unwrap_or(i64::MAX);
    let visible = cx.area();
    let first_row = visible.y.max(0);
    let end_row = visible
        .y
        .saturating_add(i64::from(visible.height))
        .min(bottom.saturating_add(1));
    let sides = border.sides;
    if sides.top {
        for x in 0..width {
            cx.set_cell(i32::from(x), 0, &border.set.horizontal.to_string(), style);
        }
    }
    if sides.bottom && bottom != 0 {
        for x in 0..width {
            cx.set_cell(
                i32::from(x),
                bottom,
                &border.set.horizontal.to_string(),
                style,
            );
        }
    }
    if sides.left {
        for y in first_row..end_row {
            cx.set_cell(0, y, &border.set.vertical.to_string(), style);
        }
    }
    if sides.right && right != 0 {
        for y in first_row..end_row {
            cx.set_cell(i32::from(right), y, &border.set.vertical.to_string(), style);
        }
    }
    if width > 1 && height > 1 {
        if sides.top && sides.left {
            cx.set_cell(0, 0, &border.set.top_left.to_string(), style);
        }
        if sides.top && sides.right {
            cx.set_cell(
                i32::from(right),
                0,
                &border.set.top_right.to_string(),
                style,
            );
        }
        if sides.bottom && sides.left {
            cx.set_cell(0, bottom, &border.set.bottom_left.to_string(), style);
        }
        if sides.bottom && sides.right {
            cx.set_cell(
                i32::from(right),
                bottom,
                &border.set.bottom_right.to_string(),
                style,
            );
        }
    }
}

/// Assign stable keyed identity to one child without changing its geometry.
pub struct Keyed<'a> {
    id: LayoutId,
    child: Element<'a>,
}

impl<'a> Keyed<'a> {
    /// Create a keyed identity wrapper.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, child: impl Component + 'a) -> Self {
        Self {
            id: id.into(),
            child: Element::new(child),
        }
    }
}

impl Component for Keyed<'_> {
    fn revision(&self) -> ComponentRevision {
        let own = ComponentRevision::new(stable_revision(|state| self.id.as_str().hash(state)), 0);
        own.combine(self.child.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let child = self.child.layout(constraints, cx);
        LayoutNode::with_children(
            self.id.clone(),
            child.size,
            vec![ChildLayout::new(0, 0, child)],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        paint_single_child(&self.child, layout, cx);
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        let Some(child) = layout.children.first() else {
            return crate::event::EventOutcome::Ignored;
        };
        self.child.event(event, &child.node, cx)
    }
}

/// Add measurable insets around one child without introducing visual chrome.
pub struct Padding<'a> {
    surface: Surface<'a>,
}

impl<'a> Padding<'a> {
    /// Create a padding wrapper.
    #[must_use]
    pub fn new(insets: Insets, child: impl Component + 'a) -> Self {
        Self {
            surface: Surface::new(child).id("padding").padding(insets),
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.surface = self.surface.id(id);
        self
    }
}

impl Component for Padding<'_> {
    fn revision(&self) -> ComponentRevision {
        self.surface.revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.surface.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.surface.paint(layout, cx);
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        self.surface.event(event, layout, cx)
    }
}

/// Fill one complete measured rectangle behind a child.
pub struct Fill<'a> {
    surface: Surface<'a>,
}

impl<'a> Fill<'a> {
    /// Create a complete rectangular fill wrapper.
    #[must_use]
    pub fn new(style: Style, child: impl Component + 'a) -> Self {
        Self {
            surface: Surface::new(child).id("fill").background(style),
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.surface = self.surface.id(id);
        self
    }
}

impl Component for Fill<'_> {
    fn revision(&self) -> ComponentRevision {
        self.surface.revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.surface.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.surface.paint(layout, cx);
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        self.surface.event(event, layout, cx)
    }
}

/// Constrain one child with optional fixed/minimum/maximum dimensions.
pub struct SizeBox<'a> {
    id: LayoutId,
    child: Element<'a>,
    width: Option<u16>,
    height: Option<usize>,
    min_width: u16,
    max_width: Option<u16>,
    min_height: usize,
    max_height: Option<usize>,
}

impl<'a> SizeBox<'a> {
    /// Create a size-constraining wrapper.
    #[must_use]
    pub fn new(child: impl Component + 'a) -> Self {
        Self {
            id: LayoutId::new("size"),
            child: Element::new(child),
            width: None,
            height: None,
            min_width: 0,
            max_width: None,
            min_height: 0,
            max_height: None,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Require an exact width, clamped by parent constraints.
    #[must_use]
    pub const fn width(mut self, width: u16) -> Self {
        self.width = Some(width);
        self
    }

    /// Require an exact logical height, clamped by parent constraints.
    #[must_use]
    pub const fn height(mut self, height: usize) -> Self {
        self.height = Some(height);
        self
    }

    /// Set a minimum width.
    #[must_use]
    pub const fn min_width(mut self, width: u16) -> Self {
        self.min_width = width;
        self
    }

    /// Set a maximum width.
    #[must_use]
    pub const fn max_width(mut self, width: u16) -> Self {
        self.max_width = Some(width);
        self
    }

    /// Set a minimum logical height.
    #[must_use]
    pub const fn min_height(mut self, height: usize) -> Self {
        self.min_height = height;
        self
    }

    /// Set a maximum logical height.
    #[must_use]
    pub const fn max_height(mut self, height: usize) -> Self {
        self.max_height = Some(height);
        self
    }
}

impl Component for SizeBox<'_> {
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(
            stable_revision(|state| {
                self.id.as_str().hash(state);
                self.width.hash(state);
                self.height.hash(state);
                self.min_width.hash(state);
                self.max_width.hash(state);
                self.min_height.hash(state);
                self.max_height.hash(state);
            }),
            0,
        )
        .combine(self.child.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let parent_min_width = constraints.min_width();
        let parent_max_width = constraints.max_width();
        let parent_min_height = constraints.min_height();
        let parent_max_height = constraints.max_height();
        let max_width = self
            .max_width
            .unwrap_or(parent_max_width)
            .min(parent_max_width);
        let min_width = self.min_width.max(parent_min_width).min(max_width);
        let max_height = match (self.max_height, parent_max_height) {
            (Some(own), Some(parent)) => Some(own.min(parent)),
            (Some(own), None) => Some(own),
            (None, parent) => parent,
        };
        let min_height = self.min_height.max(parent_min_height);
        let child_constraints = match (self.width, self.height) {
            (Some(width), Some(height)) => {
                let width = width.clamp(min_width, max_width);
                let height = max_height
                    .map_or(height, |maximum| height.min(maximum))
                    .max(min_height);
                Constraints::new(width, width, height, Some(height))
            }
            (Some(width), None) => {
                let width = width.clamp(min_width, max_width);
                Constraints::new(width, width, min_height, max_height)
            }
            (None, Some(height)) => {
                let height = max_height
                    .map_or(height, |maximum| height.min(maximum))
                    .max(min_height);
                Constraints::new(min_width, max_width, height, Some(height))
            }
            (None, None) => Constraints::new(min_width, max_width, min_height, max_height),
        };
        let child = self.child.layout(child_constraints, cx);
        LayoutNode::with_children(
            self.id.clone(),
            child.size,
            vec![ChildLayout::new(0, 0, child)],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        paint_single_child(&self.child, layout, cx);
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        event_single_child(&self.child, event, layout, cx)
    }
}

/// Align one intrinsically measured child inside the parent-assigned rectangle.
pub struct Align<'a> {
    id: LayoutId,
    child: Element<'a>,
    horizontal: HorizontalAlignment,
    vertical: VerticalAlignment,
}

impl<'a> Align<'a> {
    /// Create an alignment wrapper.
    #[must_use]
    pub fn new(child: impl Component + 'a) -> Self {
        Self {
            id: LayoutId::new("align"),
            child: Element::new(child),
            horizontal: HorizontalAlignment::Start,
            vertical: VerticalAlignment::Start,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set horizontal alignment.
    #[must_use]
    pub const fn horizontal(mut self, alignment: HorizontalAlignment) -> Self {
        self.horizontal = alignment;
        self
    }

    /// Set vertical alignment.
    #[must_use]
    pub const fn vertical(mut self, alignment: VerticalAlignment) -> Self {
        self.vertical = alignment;
        self
    }
}

impl Component for Align<'_> {
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(
            stable_revision(|state| {
                self.id.as_str().hash(state);
                (match self.horizontal {
                    HorizontalAlignment::Start => 0u8,
                    HorizontalAlignment::Center => 1,
                    HorizontalAlignment::End => 2,
                    HorizontalAlignment::Stretch => 3,
                })
                .hash(state);
                (match self.vertical {
                    VerticalAlignment::Start => 0u8,
                    VerticalAlignment::Center => 1,
                    VerticalAlignment::End => 2,
                    VerticalAlignment::Stretch => 3,
                })
                .hash(state);
            }),
            0,
        )
        .combine(self.child.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let child_constraints = Constraints::new(
            if self.horizontal == HorizontalAlignment::Stretch {
                constraints.max_width()
            } else {
                0
            },
            constraints.max_width(),
            if self.vertical == VerticalAlignment::Stretch {
                constraints
                    .max_height()
                    .unwrap_or_else(|| constraints.min_height())
            } else {
                0
            },
            constraints.max_height(),
        );
        let child = self.child.layout(child_constraints, cx);
        let size = constraints.constrain(LogicalSize::new(
            constraints.max_width(),
            constraints.max_height().unwrap_or(child.size.height),
        ));
        let x = match self.horizontal {
            HorizontalAlignment::Start | HorizontalAlignment::Stretch => 0,
            HorizontalAlignment::Center => size.width.saturating_sub(child.size.width) / 2,
            HorizontalAlignment::End => size.width.saturating_sub(child.size.width),
        };
        let y = match self.vertical {
            VerticalAlignment::Start | VerticalAlignment::Stretch => 0,
            VerticalAlignment::Center => size.height.saturating_sub(child.size.height) / 2,
            VerticalAlignment::End => size.height.saturating_sub(child.size.height),
        };
        LayoutNode::with_children(self.id.clone(), size, vec![ChildLayout::new(x, y, child)])
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        paint_single_child(&self.child, layout, cx);
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        event_single_child(&self.child, event, layout, cx)
    }
}

/// Paint one child through an explicit local clip.
pub struct Clip<'a> {
    id: LayoutId,
    child: Element<'a>,
}

impl<'a> Clip<'a> {
    /// Create a clipping wrapper.
    #[must_use]
    pub fn new(child: impl Component + 'a) -> Self {
        Self {
            id: LayoutId::new("clip"),
            child: Element::new(child),
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }
}

impl Component for Clip<'_> {
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(stable_revision(|state| self.id.as_str().hash(state)), 0)
            .combine(self.child.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let child = self.child.layout(constraints, cx);
        LayoutNode::with_children(
            self.id.clone(),
            constraints.constrain(child.size),
            vec![ChildLayout::new(0, 0, child)],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        paint_single_child(&self.child, layout, cx);
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        event_single_child(&self.child, event, layout, cx)
    }
}

/// Apply inherited style to one child without changing geometry.
pub struct StyleScope<'a> {
    id: LayoutId,
    child: Element<'a>,
    style: Style,
}

impl<'a> StyleScope<'a> {
    /// Create an inherited-style wrapper.
    #[must_use]
    pub fn new(child: impl Component + 'a, style: Style) -> Self {
        Self {
            id: LayoutId::new("style"),
            child: Element::new(child),
            style,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }
}

impl Component for StyleScope<'_> {
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(
            stable_revision(|state| self.id.as_str().hash(state)),
            stable_revision(|state| self.style.hash(state)),
        )
        .combine(self.child.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let child = self.child.layout(constraints, cx);
        LayoutNode::with_children(
            self.id.clone(),
            child.size,
            vec![ChildLayout::new(0, 0, child)],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        cx.with_style(self.style, |cx| paint_single_child(&self.child, layout, cx));
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        event_single_child(&self.child, event, layout, cx)
    }
}

/// Include or omit a child while retaining ordinary composition semantics.
pub struct Visibility<'a> {
    id: LayoutId,
    child: Element<'a>,
    visible: bool,
}

impl<'a> Visibility<'a> {
    /// Create a visibility wrapper.
    #[must_use]
    pub fn new(child: impl Component + 'a, visible: bool) -> Self {
        Self {
            id: LayoutId::new("visibility"),
            child: Element::new(child),
            visible,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }
}

impl Component for Visibility<'_> {
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(
            stable_revision(|state| {
                self.id.as_str().hash(state);
                self.visible.hash(state);
            }),
            0,
        )
        .combine(self.child.revision())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        if !self.visible {
            return LayoutNode::leaf(
                self.id.clone(),
                constraints.constrain(LogicalSize::default()),
            );
        }
        let child = self.child.layout(constraints, cx);
        LayoutNode::with_children(
            self.id.clone(),
            child.size,
            vec![ChildLayout::new(0, 0, child)],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if self.visible {
            paint_single_child(&self.child, layout, cx);
        }
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        if self.visible {
            event_single_child(&self.child, event, layout, cx)
        } else {
            crate::event::EventOutcome::Ignored
        }
    }
}

/// Overlay children in deterministic insertion/paint order.
pub struct Stack<'a> {
    id: LayoutId,
    children: Vec<Element<'a>>,
}

impl<'a> Stack<'a> {
    /// Create an empty overlay stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: LayoutId::new("stack"),
            children: Vec::new(),
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Append one overlay child.
    #[must_use]
    pub fn child(mut self, child: impl Component + 'a) -> Self {
        self.children.push(Element::new(child));
        self
    }
}

impl Default for Stack<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Stack<'_> {
    fn revision(&self) -> ComponentRevision {
        combine_child_revisions(
            ComponentRevision::new(stable_revision(|state| self.id.as_str().hash(state)), 0),
            self.children.iter().map(Element::revision),
        )
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let mut width = constraints.min_width();
        let mut height = constraints.min_height();
        let children = self
            .children
            .iter()
            .map(|child| {
                let node = child.layout(constraints, cx);
                width = width.max(node.size.width);
                height = height.max(node.size.height);
                ChildLayout::new(0, 0, node)
            })
            .collect();
        LayoutNode::with_children(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
            children,
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (child, resolved) in self.children.iter().zip(&layout.children) {
            paint_child(child, resolved, cx);
        }
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        event_children(&self.children, event, layout, cx)
    }
}

fn paint_single_child(child: &Element<'_>, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
    if let Some(resolved) = layout.children.first() {
        paint_child(child, resolved, cx);
    }
}

fn paint_child(child: &Element<'_>, resolved: &ChildLayout, cx: &mut PaintCx<'_, '_>) {
    cx.with_child_size(
        i32::from(resolved.x),
        i64::try_from(resolved.y).unwrap_or(i64::MAX),
        resolved.node.size,
        |cx| child.paint(&resolved.node, cx),
    );
}

/// One child in a horizontal row.
struct RowChild<'a> {
    component: Element<'a>,
    flex: u16,
}

/// A child requesting a weighted share of a [`Row`]'s remaining width.
///
/// `Flex` is a composition descriptor rather than an independent layout node,
/// so it does not introduce identity or geometry between the row and child.
pub struct Flex<'a> {
    component: Element<'a>,
    weight: u16,
}

impl<'a> Flex<'a> {
    /// Wrap a child with a positive flex weight. Zero is normalized to one.
    #[must_use]
    pub fn new(weight: u16, child: impl Component + 'a) -> Self {
        Self {
            component: Element::new(child),
            weight: weight.max(1),
        }
    }

    /// Return the normalized allocation weight.
    #[must_use]
    pub const fn weight(&self) -> u16 {
        self.weight
    }
}

/// Horizontal composition supporting intrinsic and proportionally flexible children.
pub struct Row<'a> {
    id: LayoutId,
    children: Vec<RowChild<'a>>,
    gap: u16,
    alignment: VerticalAlignment,
}

impl<'a> Row<'a> {
    /// Create an empty row.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: LayoutId::new("row"),
            children: Vec::new(),
            gap: 0,
            alignment: VerticalAlignment::Start,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set cells between children.
    #[must_use]
    pub const fn gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    /// Set cross-axis child alignment.
    #[must_use]
    pub const fn alignment(mut self, alignment: VerticalAlignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Append an intrinsically sized child.
    #[must_use]
    pub fn child(mut self, child: impl Component + 'a) -> Self {
        self.children.push(RowChild {
            component: Element::new(child),
            flex: 0,
        });
        self
    }

    /// Append a child receiving a proportional share of remaining width.
    #[must_use]
    pub fn flex(mut self, child: Flex<'a>) -> Self {
        self.children.push(RowChild {
            component: child.component,
            flex: child.weight,
        });
        self
    }

    /// Append a child receiving a proportional share of remaining width.
    #[must_use]
    pub fn flex_child(self, weight: u16, child: impl Component + 'a) -> Self {
        self.flex(Flex::new(weight, child))
    }
}

impl Default for Row<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Row<'_> {
    fn revision(&self) -> ComponentRevision {
        let own = ComponentRevision::new(
            stable_revision(|state| {
                self.id.as_str().hash(state);
                self.gap.hash(state);
                (match self.alignment {
                    VerticalAlignment::Start => 0u8,
                    VerticalAlignment::Center => 1,
                    VerticalAlignment::End => 2,
                    VerticalAlignment::Stretch => 3,
                })
                .hash(state);
                for child in &self.children {
                    child.flex.hash(state);
                }
            }),
            0,
        );
        combine_child_revisions(
            own,
            self.children.iter().map(|child| child.component.revision()),
        )
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let gaps = self.gap.saturating_mul(
            u16::try_from(self.children.len().saturating_sub(1)).unwrap_or(u16::MAX),
        );
        let available = constraints.max_width().saturating_sub(gaps);
        let mut resolved: Vec<Option<LayoutNode>> = vec![None; self.children.len()];
        let mut intrinsic_width = 0u16;
        let mut flex_weight = 0u32;
        for (index, child) in self.children.iter().enumerate() {
            if child.flex == 0 {
                let node = child.component.layout(
                    Constraints::new(0, available, 0, constraints.max_height()),
                    cx,
                );
                intrinsic_width = intrinsic_width.saturating_add(node.size.width);
                resolved[index] = Some(node);
            } else {
                flex_weight = flex_weight.saturating_add(u32::from(child.flex));
            }
        }
        let remaining = available.saturating_sub(intrinsic_width);
        let mut assigned_flex = 0u16;
        let mut seen_weight = 0u32;
        for (index, child) in self.children.iter().enumerate() {
            if child.flex == 0 {
                continue;
            }
            seen_weight = seen_weight.saturating_add(u32::from(child.flex));
            let cumulative = u32::from(remaining)
                .saturating_mul(seen_weight)
                .checked_div(flex_weight.max(1))
                .unwrap_or(0);
            let cumulative = u16::try_from(cumulative).unwrap_or(u16::MAX);
            let width = cumulative.saturating_sub(assigned_flex);
            assigned_flex = cumulative;
            resolved[index] = Some(child.component.layout(
                Constraints::new(width, width, 0, constraints.max_height()),
                cx,
            ));
        }
        let mut x = 0u16;
        let mut height = 0usize;
        let mut children = Vec::with_capacity(self.children.len());
        for node in resolved.into_iter().flatten() {
            height = height.max(node.size.height);
            let width = node.size.width;
            children.push(ChildLayout::new(x, 0, node));
            x = x.saturating_add(width).saturating_add(self.gap);
        }
        if !children.is_empty() {
            x = x.saturating_sub(self.gap);
        }
        let size = constraints.constrain(LogicalSize::new(x, height));
        for (index, child) in children.iter_mut().enumerate() {
            child.y = match self.alignment {
                VerticalAlignment::Start => 0,
                VerticalAlignment::Center => size.height.saturating_sub(child.node.size.height) / 2,
                VerticalAlignment::End => size.height.saturating_sub(child.node.size.height),
                VerticalAlignment::Stretch => {
                    let width = child.node.size.width;
                    child.node = self.children[index].component.layout(
                        Constraints::new(width, width, size.height, Some(size.height)),
                        cx,
                    );
                    0
                }
            };
        }
        LayoutNode::with_children(self.id.clone(), size, children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (child, resolved) in self.children.iter().zip(&layout.children) {
            paint_child(&child.component, resolved, cx);
        }
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        self.children
            .iter()
            .rev()
            .zip(layout.children.iter().rev())
            .find_map(|(child, resolved)| {
                let outcome = cx.with_child(resolved, |cx| {
                    child.component.event(event, &resolved.node, cx)
                });
                outcome.is_handled().then_some(outcome)
            })
            .unwrap_or(crate::event::EventOutcome::Ignored)
    }
}

/// Vertical composition of variable-height children.
pub struct Column<'a> {
    id: LayoutId,
    children: Vec<Element<'a>>,
    weights: Vec<u16>,
    gap: usize,
    alignment: HorizontalAlignment,
}

impl<'a> Column<'a> {
    /// Create an empty column.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: LayoutId::new("column"),
            children: Vec::new(),
            weights: Vec::new(),
            gap: 0,
            alignment: HorizontalAlignment::Stretch,
        }
    }

    /// Set stable layout identity.
    #[must_use]
    pub fn id(mut self, id: impl Into<LayoutId>) -> Self {
        self.id = id.into();
        self
    }

    /// Set logical rows between children.
    #[must_use]
    pub const fn gap(mut self, gap: usize) -> Self {
        self.gap = gap;
        self
    }

    /// Set cross-axis child alignment. Columns stretch children by default.
    #[must_use]
    pub const fn alignment(mut self, alignment: HorizontalAlignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Append a child.
    #[must_use]
    pub fn child(mut self, child: impl Component + 'a) -> Self {
        self.children.push(Element::new(child));
        self.weights.push(0);
        self
    }

    /// Append a child receiving a weighted share of remaining bounded height.
    /// With unbounded height, flexible children retain their intrinsic height.
    #[must_use]
    pub fn flex(mut self, child: Flex<'a>) -> Self {
        self.children.push(child.component);
        self.weights.push(child.weight);
        self
    }
}

impl Default for Column<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Column<'_> {
    fn revision(&self) -> ComponentRevision {
        let own = ComponentRevision::new(
            stable_revision(|state| {
                self.id.as_str().hash(state);
                self.gap.hash(state);
                self.weights.hash(state);
                (match self.alignment {
                    HorizontalAlignment::Start => 0u8,
                    HorizontalAlignment::Center => 1,
                    HorizontalAlignment::End => 2,
                    HorizontalAlignment::Stretch => 3,
                })
                .hash(state);
            }),
            0,
        );
        combine_child_revisions(own, self.children.iter().map(Element::revision))
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let mut y = 0usize;
        let width = constraints.max_width();
        let mut children = Vec::with_capacity(self.children.len());
        let total_weight = self
            .weights
            .iter()
            .map(|weight| usize::from(*weight))
            .sum::<usize>();
        let flex_height = constraints
            .max_height()
            .filter(|_| total_weight > 0)
            .map(|height| {
                let fixed = self
                    .children
                    .iter()
                    .zip(&self.weights)
                    .filter(|(_, weight)| **weight == 0)
                    .map(|(child, _)| {
                        child
                            .layout(Constraints::new(width, width, 0, None), cx)
                            .size
                            .height
                    })
                    .sum::<usize>();
                height.saturating_sub(fixed).saturating_sub(
                    self.gap
                        .saturating_mul(self.children.len().saturating_sub(1)),
                )
            });
        let mut allocated = 0usize;
        let mut consumed_weight = 0usize;
        for (child, weight) in self.children.iter().zip(&self.weights) {
            let child_min_width = if self.alignment == HorizontalAlignment::Stretch {
                width
            } else {
                0
            };
            let child_constraints = flex_height.filter(|_| *weight > 0).map_or_else(
                || Constraints::new(child_min_width, width, 0, constraints.max_height()),
                |height| {
                    consumed_weight += usize::from(*weight);
                    let end = height.saturating_mul(consumed_weight) / total_weight;
                    let share = end.saturating_sub(allocated);
                    allocated = end;
                    Constraints::new(child_min_width, width, share, Some(share))
                },
            );
            let node = child.layout(child_constraints, cx);
            let x = match self.alignment {
                HorizontalAlignment::Start | HorizontalAlignment::Stretch => 0,
                HorizontalAlignment::Center => width.saturating_sub(node.size.width) / 2,
                HorizontalAlignment::End => width.saturating_sub(node.size.width),
            };
            children.push(ChildLayout::new(x, y, node));
            y = y
                .saturating_add(children.last().map_or(0, |child| child.node.size.height))
                .saturating_add(self.gap);
        }
        if !children.is_empty() {
            y = y.saturating_sub(self.gap);
        }
        let size = constraints.constrain(LogicalSize::new(width, y));
        LayoutNode::with_children(self.id.clone(), size, children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (component, child) in self.children.iter().zip(&layout.children) {
            paint_child(component, child, cx);
        }
    }

    fn event(
        &self,
        event: &crate::event::Event,
        layout: &LayoutNode,
        cx: &mut crate::component::EventCx<'_>,
    ) -> crate::event::EventOutcome {
        event_children(&self.children, event, layout, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Align, Clip, Column, Fill, Flex, HorizontalAlignment, Keyed, Padding, Row, SizeBox, Stack,
        StyleScope, Surface, TextBlock, VerticalAlignment, Visibility,
    };
    use crate::buffer::Buffer;
    use crate::chrome::Border;
    use crate::component::{
        Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode,
        LogicalSize,
    };
    use crate::event::{Event, EventOutcome};
    use crate::frame::Frame;
    use crate::geometry::{Insets, Point, Rect};
    use crate::paint::PaintCx;
    use crate::style::{Color, Style};

    struct EventLeaf {
        id: &'static str,
        outcome: EventOutcome,
    }

    impl Component for EventLeaf {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new(self.id),
                constraints.constrain(LogicalSize::new(1, 1)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
            if matches!(event, Event::User(value) if value == self.id)
                && cx.find(&layout.id).is_some()
            {
                self.outcome
            } else {
                EventOutcome::Ignored
            }
        }
    }

    #[test]
    fn full_width_message_card_composes_without_precomputed_height() {
        let card = Surface::new(
            Column::new()
                .gap(1)
                .child(
                    Row::new()
                        .gap(1)
                        .child(TextBlock::new("Alice"))
                        .flex_child(1, TextBlock::new("10:42")),
                )
                .child(TextBlock::new(
                    "A long message wraps naturally from constraints without a caller-owned height.",
                )),
        )
        .id("message-card")
        .background(Style::new().bg(Color::Blue))
        .border(Border::single())
        .padding(Insets::all(1));
        let mut cx = LayoutCx::new();
        let layout = card.layout(Constraints::for_width(24), &mut cx);
        let area = Rect::new(
            0,
            0,
            layout.size.width,
            u16::try_from(layout.size.height).unwrap(),
        );
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        card.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(layout.size.width, 24);
        assert!(layout.size.height > 5);
        for y in 0..area.height {
            assert_eq!(
                frame.buffer().get(Point::new(0, y)).unwrap().style.bg,
                Some(Color::Blue)
            );
            assert_eq!(
                frame
                    .buffer()
                    .get(Point::new(area.width - 1, y))
                    .unwrap()
                    .style
                    .bg,
                Some(Color::Blue)
            );
        }
    }

    #[test]
    fn nested_composition_routes_events_through_authoritative_layout() {
        let component = Surface::new(
            Padding::new(
                Insets::all(1),
                Column::new().child(StyleScope::new(
                    Keyed::new(
                        "action-key",
                        EventLeaf {
                            id: "action",
                            outcome: EventOutcome::Redraw,
                        },
                    ),
                    Style::new().fg(Color::Green),
                )),
            )
            .id("padding"),
        )
        .id("surface");
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let mut event_cx = EventCx::new(&layout);

        assert_eq!(
            component.event(&Event::User("action".into()), &layout, &mut event_cx),
            EventOutcome::Redraw
        );
    }

    #[test]
    fn overlay_event_routing_prefers_topmost_handled_child() {
        let component = Stack::new()
            .child(EventLeaf {
                id: "action",
                outcome: EventOutcome::Handled,
            })
            .child(EventLeaf {
                id: "action",
                outcome: EventOutcome::Redraw,
            });
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let mut event_cx = EventCx::new(&layout);

        assert_eq!(
            component.event(&Event::User("action".into()), &layout, &mut event_cx),
            EventOutcome::Redraw
        );
    }

    #[test]
    fn hidden_children_do_not_receive_events() {
        let component = Visibility::new(
            EventLeaf {
                id: "action",
                outcome: EventOutcome::Handled,
            },
            false,
        );
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let mut event_cx = EventCx::new(&layout);

        assert_eq!(
            component.event(&Event::User("action".into()), &layout, &mut event_cx),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn cached_text_geometry_survives_all_paint_only_channels() {
        use crate::component::LayoutCache;
        use crate::text::{Line, Span, Text, TextWrap};
        use crate::text_block::Alignment;

        let original = TextBlock::new("ab cd\n界ef").wrap(TextWrap::Word);
        let changed = TextBlock::new(Text::from_lines([
            Line::from_spans([Span::styled("ab cd", Style::new().fg(Color::Green))]),
            Line::raw("界ef"),
        ]))
        .wrap(TextWrap::Word);
        let mut cache = LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::for_width(4);
        let original_layout = cache.layout(LayoutId::new("text"), &original, constraints, &mut cx);
        let measured = cx.measured_nodes();
        for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
            for scroll in [0, 1, usize::MAX] {
                let component = changed
                    .clone()
                    .alignment(alignment)
                    .vertical_scroll(scroll)
                    .style(Style::new().fg(Color::Blue).bg(Color::Red));
                assert_eq!(original.revision().layout, component.revision().layout);
                let cached = cache.layout(LayoutId::new("text"), &component, constraints, &mut cx);
                assert_eq!(cx.measured_nodes(), measured);
                assert!(std::sync::Arc::ptr_eq(
                    original_layout.metadata.text_rows.as_ref().unwrap(),
                    cached.metadata.text_rows.as_ref().unwrap(),
                ));
                let fresh = component.layout(constraints, &mut LayoutCx::new());
                let render = |layout: &LayoutNode| {
                    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 3));
                    let mut frame = Frame::new(&mut buffer);
                    component.paint(layout, &mut PaintCx::new(&mut frame));
                    component.register_selection(
                        layout,
                        &mut PaintCx::new(&mut frame),
                        "scope",
                        "text",
                        0,
                        1,
                    );
                    let fragments = frame.selection().fragments().to_vec();
                    (buffer, fragments)
                };
                assert_eq!(render(&cached), render(&fresh));
                assert_eq!(component.projection(&cached), component.projection(&fresh));
            }
        }
    }

    #[test]
    fn retained_text_rows_use_current_paint_styles_without_rewrapping() {
        use crate::text::{Line, Span, Text};
        let original = TextBlock::new(Text::from_lines([Line::from_spans([
            Span::styled("ab", Style::new().fg(Color::Red)),
            Span::raw("cd"),
        ])]));
        let changed = TextBlock::new(Text::from_lines([Line::from_spans([
            Span::styled("a", Style::new().fg(Color::Blue)),
            Span::styled("bcd", Style::new().fg(Color::Green)),
        ])]));
        let layout = original.layout(Constraints::for_width(2), &mut LayoutCx::new());
        let rows = changed.measured_rows(&layout, 1).collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].line.plain_text(), "cd");
        assert_eq!(rows[0].source_range, 2..4);
        assert_eq!(rows[0].line.spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn incremental_projection_preserves_offsets_across_source_lines() {
        let component = TextBlock::new("abcd\né🙂\nz").wrap(crate::text::TextWrap::Character);
        let mut rows = component.projection_rows(2);
        for (text, source_line, range) in [
            ("ab", 0, 0..2),
            ("cd", 0, 2..4),
            ("é", 1, 5..7),
            ("🙂", 1, 7..11),
            ("z", 2, 12..13),
        ] {
            let row = rows.next().unwrap();
            assert_eq!(row.line.plain_text(), text);
            assert_eq!(row.source_line, source_line);
            assert_eq!(row.source_range, range);
        }
        assert!(rows.next().is_none());
    }

    #[test]
    fn measured_height_matches_projection_for_text_policies() {
        for text in [
            "",
            "\n",
            "a\n\n",
            "one two  three",
            "é🙂中 a",
            "a\t b\nlast  ",
        ] {
            for wrap in [
                crate::text::TextWrap::None,
                crate::text::TextWrap::Word,
                crate::text::TextWrap::Character,
            ] {
                for trim in [false, true] {
                    for width in [0, 1, 2, 5, 20] {
                        let component = TextBlock::new(text).wrap(wrap).trim(trim);
                        let layout =
                            component.layout(Constraints::for_width(width), &mut LayoutCx::new());
                        assert_eq!(
                            layout.size.height,
                            component
                                .projection(
                                    &component.layout(
                                        Constraints::for_width(width),
                                        &mut LayoutCx::new()
                                    )
                                )
                                .len(),
                            "text={text:?}, wrap={wrap:?}, trim={trim}, width={width}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn source_start_preserves_projection_suffixes() {
        use crate::text::TextWrap;

        for text in ["", "\n", "a\n\n", "one two   three\n界🙂 abcdef\n  last  "] {
            for wrap in [TextWrap::None, TextWrap::Character, TextWrap::Word] {
                for trim in [false, true] {
                    for width in [0, 1, 2, 5, 20] {
                        let block = TextBlock::new(text).wrap(wrap).trim(trim);
                        let all = block.projection(
                            &block.layout(Constraints::for_width(width), &mut LayoutCx::new()),
                        );
                        for first in (0..=all.len()).chain([usize::MAX]) {
                            let (source, offset) = block.source_start(width, first);
                            let suffix: Vec<_> = block
                                .projection_rows_from(width, source)
                                .skip(offset)
                                .collect();
                            let expected = &all[first.min(all.len())..];
                            assert_eq!(suffix.len(), expected.len());
                            for (actual, expected) in suffix.iter().zip(expected) {
                                assert_eq!(actual.line.plain_text(), expected.line.plain_text());
                                assert_eq!(actual.source_line, expected.source_line);
                                assert_eq!(actual.source_range, expected.source_range);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn text_visible_rows_intersect_layout_and_parent_clip() {
        let component = TextBlock::new("a\nb\nc\nd");
        let layout = component.layout(Constraints::for_width(2), &mut LayoutCx::new());
        for (clip, expected) in [
            (crate::paint::LocalRect::new(0, 2, 2, 1), 2..3),
            (crate::paint::LocalRect::new(0, -2, 2, 3), 0..1),
            (crate::paint::LocalRect::new(0, 3, 2, 8), 3..4),
            (crate::paint::LocalRect::new(2, 0, 1, 4), 0..0),
            (crate::paint::LocalRect::new(-2, 0, 2, 4), 0..0),
            (crate::paint::LocalRect::new(0, 0, 2, 0), 0..0),
        ] {
            assert_eq!(TextBlock::visible_rows(&layout, clip), expected);
        }
    }

    #[test]
    fn unwrapped_text_scroll_at_usize_limit_paints_nothing() {
        let component = TextBlock::new("a\nb")
            .wrap(crate::text::TextWrap::None)
            .vertical_scroll(usize::MAX);
        let layout = component.layout(Constraints::for_width(1), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            0,
            -1,
            crate::paint::LocalRect::new(0, 1, 1, 1),
            |cx| component.paint(&layout, cx),
        );
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" "));
    }

    #[test]
    fn selection_matches_scrolled_text_beyond_terminal_row_limit() {
        let component = TextBlock::new(format!("{}z", "a\n".repeat(70_000))).vertical_scroll(1);
        let layout = component.layout(Constraints::for_width(1), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            0,
            -69_999,
            crate::paint::LocalRect::new(0, 69_999, 1, 1),
            |cx| {
                component.paint(&layout, cx);
                component.register_selection(&layout, cx, "scope", "content", 0, 1);
            },
        );
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("z"));
        let fragments = frame.selection().fragments();
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].area, Rect::new(0, 0, 1, 1));
        assert_eq!(fragments[0].source_range, 140_000..140_001);
    }

    #[test]
    fn scrolled_text_matches_full_paint_and_selection_for_all_policies() {
        use crate::text::TextWrap;
        use crate::text_block::Alignment;

        for wrap in [TextWrap::None, TextWrap::Character, TextWrap::Word] {
            for trim in [false, true] {
                for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
                    let component = TextBlock::new("head\n界ab cd  \n\ne\u{301}fg hi\ntail")
                        .wrap(wrap)
                        .trim(trim)
                        .alignment(alignment);
                    let layout = component.layout(Constraints::for_width(5), &mut LayoutCx::new());
                    let height = u16::try_from(layout.size.height).unwrap();
                    let mut full_buffer = Buffer::empty(Rect::new(0, 0, 5, height));
                    let mut full = Frame::new(&mut full_buffer);
                    component.paint(&layout, &mut PaintCx::new(&mut full));
                    component.register_selection(
                        &layout,
                        &mut PaintCx::new(&mut full),
                        "scope",
                        "content",
                        0,
                        1,
                    );
                    for first in 1..height {
                        let scrolled = component.clone().vertical_scroll(1);
                        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
                        let mut frame = Frame::new(&mut buffer);
                        let local_row = i64::from(first - 1);
                        PaintCx::new(&mut frame).with_child(
                            0,
                            -local_row,
                            crate::paint::LocalRect::new(0, local_row, 5, 1),
                            |cx| {
                                scrolled.paint(&layout, cx);
                                scrolled.register_selection(&layout, cx, "scope", "content", 0, 1);
                            },
                        );
                        assert_eq!(
                            frame.buffer().row_symbols(0),
                            full.buffer().row_symbols(first)
                        );
                        let expected: Vec<_> = full
                            .selection()
                            .fragments()
                            .iter()
                            .filter(|fragment| fragment.area.y == first)
                            .map(|fragment| {
                                (
                                    fragment.area.x,
                                    fragment.area.width,
                                    fragment.source_range.clone(),
                                )
                            })
                            .collect();
                        let actual: Vec<_> = frame
                            .selection()
                            .fragments()
                            .iter()
                            .map(|fragment| {
                                assert_eq!(fragment.area.y, 0);
                                (
                                    fragment.area.x,
                                    fragment.area.width,
                                    fragment.source_range.clone(),
                                )
                            })
                            .collect();
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn text_selection_uses_authoritative_projection_and_alignment() {
        let component = TextBlock::new("one two").alignment(crate::text_block::Alignment::Right);
        let layout = component.layout(Constraints::for_width(6), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);
        component.register_selection(
            &layout,
            &mut PaintCx::new(&mut frame),
            "messages",
            "message:1",
            10,
            3,
        );
        let fragments = frame.selection().fragments();
        assert_eq!(fragments.len(), 7);
        assert_eq!(fragments[0].area, Rect::new(2, 0, 1, 1));
        assert_eq!(fragments[0].source_range, 0..1);
        assert_eq!(fragments[4].area, Rect::new(3, 1, 1, 1));
        assert_eq!(fragments[4].source_range, 4..5);
        assert!(fragments.iter().all(|fragment| fragment.revision == 3));
    }

    #[test]
    fn wrapped_utf8_selection_survives_nested_scroll_clipping() {
        let component = TextBlock::new("aé🙂b");
        let layout = component.layout(Constraints::for_width(2), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(4, 7, 2, 1));
        let mut frame = Frame::new(&mut buffer);
        let mut paint = PaintCx::new(&mut frame);
        paint.with_child(4, 6, crate::paint::LocalRect::new(0, 1, 2, 1), |cx| {
            component.register_selection(&layout, cx, "messages", "message:1", 10, 3);
        });

        let fragments = frame.selection().fragments();
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].scope_id.as_str(), "messages");
        assert_eq!(fragments[0].content_id.as_str(), "message:1");
        assert_eq!(fragments[0].area, Rect::new(4, 7, 2, 1));
        assert_eq!(fragments[0].source_range, 3..7);
        assert_eq!(fragments[0].revision, 3);
    }

    #[test]
    fn trimmed_character_rows_advance_over_consumed_whitespace() {
        let component = TextBlock::new("a   b")
            .wrap(crate::text::TextWrap::Character)
            .trim(true);
        let rows = component
            .projection(&component.layout(Constraints::for_width(2), &mut LayoutCx::new()));
        assert_eq!(
            rows.iter()
                .map(|row| row.line.plain_text())
                .collect::<Vec<_>>(),
            ["a", "", "b"]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.source_range.clone())
                .collect::<Vec<_>>(),
            [0..1, 2..2, 4..5]
        );
    }

    #[test]
    fn character_measurement_preserves_large_logical_height() {
        let component = TextBlock::new("界".repeat(70_000)).wrap(crate::text::TextWrap::Character);
        for (width, expected) in [(1, 70_000), (2, 70_000), (4, 35_000), (3, 70_000)] {
            let layout = component.layout(Constraints::for_width(width), &mut LayoutCx::new());
            assert_eq!(layout.size.height, expected);
            assert_eq!(layout.size.width, width);
        }
        let empty = TextBlock::new(crate::text::Text::from_lines(vec![]))
            .wrap(crate::text::TextWrap::Character);
        assert_eq!(empty.row_count(0), 0);
        assert_eq!(
            TextBlock::new("")
                .wrap(crate::text::TextWrap::Character)
                .row_count(0),
            1
        );
    }

    #[test]
    fn intrinsic_text_width_respects_changing_constraints() {
        let component = TextBlock::new("界ab\nx").wrap(crate::text::TextWrap::None);
        for (min, max, expected) in [(0, 10, 4), (0, 2, 2), (6, 10, 6), (0, 10, 4)] {
            let layout =
                component.layout(Constraints::new(min, max, 0, None), &mut LayoutCx::new());
            assert_eq!(layout.size.width, expected);
            assert_eq!(layout.size.height, 2);
        }
        let wide = TextBlock::new("x".repeat(70_000)).wrap(crate::text::TextWrap::None);
        assert_eq!(
            wide.layout(Constraints::new(0, u16::MAX, 0, None), &mut LayoutCx::new())
                .size
                .width,
            u16::MAX
        );
    }

    #[test]
    fn immutable_text_fingerprints_preserve_revision_channels() {
        let plain = TextBlock::new("hello");
        let changed = TextBlock::new("world");
        let styled = TextBlock::new(crate::text::Text::from_lines(vec![
            crate::text::Line::from_spans(vec![crate::text::Span::styled(
                "hello",
                Style::new().fg(Color::Green),
            )]),
        ]));
        let restyled = plain.clone().style(Style::new().fg(Color::Blue));
        assert_eq!(plain.revision().layout, restyled.revision().layout);
        assert_ne!(plain.revision().paint, restyled.revision().paint);
        assert_eq!(plain.revision(), TextBlock::new("hello").revision());
        assert_ne!(plain.revision().layout, changed.revision().layout);
        assert_eq!(plain.revision().paint, changed.revision().paint);
        assert_eq!(plain.revision().layout, styled.revision().layout);
        assert_ne!(plain.revision().paint, styled.revision().paint);
    }

    #[test]
    fn source_line_index_handles_empty_and_trailing_lines() {
        for source in ["", "\n", "界\n\n", "a\ne\u{301}\nlast"] {
            let component = TextBlock::new(source).wrap(crate::text::TextWrap::None);
            let rows = component
                .projection(&component.layout(Constraints::for_width(10), &mut LayoutCx::new()));
            let mut expected_offset = 0;
            for (index, line) in component.text().lines.iter().enumerate() {
                assert_eq!(rows[index].source_range.start, expected_offset);
                assert_eq!(
                    component
                        .projection_rows_from(10, index)
                        .next()
                        .unwrap()
                        .source_range,
                    rows[index].source_range
                );
                expected_offset += line.plain_text().len() + 1;
            }
            assert!(
                component
                    .projection_rows_from(10, rows.len())
                    .next()
                    .is_none()
            );
        }
    }

    #[test]
    fn projection_suffix_preserves_styled_unicode_source_offsets() {
        let component = TextBlock::new(crate::text::Text::from_lines(vec![
            crate::text::Line::from_spans(vec![
                crate::text::Span::raw("界"),
                crate::text::Span::styled("e\u{301}", Style::new().fg(Color::Green)),
            ]),
            crate::text::Line::raw(""),
            crate::text::Line::raw("hello  "),
            crate::text::Line::raw("end"),
        ]))
        .wrap(crate::text::TextWrap::None)
        .trim(true);
        let all = component
            .projection(&component.layout(Constraints::for_width(3), &mut LayoutCx::new()));
        for first in 0..=5 {
            let suffix: Vec<_> = component.projection_rows_from(3, first).collect();
            let expected: Vec<_> = all.iter().skip(first).collect();
            assert_eq!(suffix.len(), expected.len());
            for (actual, expected) in suffix.iter().zip(expected) {
                if actual.line.plain_text().is_empty() {
                    assert!(expected.line.plain_text().is_empty());
                } else {
                    assert_eq!(actual.line, expected.line);
                }
                assert_eq!(actual.source_line, expected.source_line);
                assert_eq!(actual.source_range, expected.source_range);
            }
        }
        assert_eq!(all[2].source_range, 8..13);
        assert!(
            component
                .projection_rows_from(3, usize::MAX)
                .next()
                .is_none()
        );
    }

    #[test]
    fn text_trim_preserves_styles_and_projects_trimmed_source_ranges() {
        let component = TextBlock::new(crate::text::Text::from_lines(vec![
            crate::text::Line::from_spans(vec![
                crate::text::Span::styled("ab", Style::new().fg(Color::Green)),
                crate::text::Span::styled("  ", Style::new().fg(Color::Blue)),
            ]),
            crate::text::Line::raw("cd  ef"),
        ]))
        .wrap(crate::text::TextWrap::Character)
        .trim(true);
        let rows = component
            .projection(&component.layout(Constraints::for_width(4), &mut LayoutCx::new()));

        assert_eq!(
            component
                .text()
                .lines
                .iter()
                .map(crate::text::Line::plain_text)
                .collect::<Vec<_>>(),
            ["ab  ", "cd  ef"]
        );
        assert_eq!(rows[0].line.plain_text(), "ab");
        assert_eq!(rows[0].line.spans.len(), 1);
        assert_eq!(rows[0].line.spans[0].style.fg, Some(Color::Green));
        assert_eq!(rows[0].source_range, 0..2);
        assert_eq!(rows[1].line.plain_text(), "cd");
        assert_eq!(rows[1].source_range, 5..7);
        assert_eq!(rows[2].line.plain_text(), "ef");
        assert_eq!(rows[2].source_range, 9..11);
    }

    #[test]
    fn word_projection_matches_across_spans_and_skipped_whitespace() {
        let component = TextBlock::new(crate::text::Text::from_lines(vec![
            crate::text::Line::from_spans(vec![
                crate::text::Span::raw("ab  "),
                crate::text::Span::styled("界", Style::new().fg(Color::Green)),
                crate::text::Span::raw("xy  ab"),
            ]),
        ]))
        .wrap(crate::text::TextWrap::Word)
        .trim(true);
        let rows = component
            .projection(&component.layout(Constraints::for_width(4), &mut LayoutCx::new()));
        assert_eq!(
            rows.iter()
                .map(|row| row.line.plain_text())
                .collect::<Vec<_>>(),
            ["ab", "界xy", "ab"]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.source_range.clone())
                .collect::<Vec<_>>(),
            [0..2, 4..9, 11..13]
        );
    }

    #[test]
    fn projection_match_preserves_overlapping_candidates_and_fallback() {
        let mut source = b"aaaab tail".iter().copied();
        assert_eq!(
            super::advance_to_match(&mut source, b"aaab".iter().copied()),
            1
        );
        assert_eq!(source.collect::<Vec<_>>(), b" tail");

        let mut source = b"abcdef".iter().copied();
        assert_eq!(
            super::advance_to_match(&mut source, b"xyz".iter().copied()),
            0
        );
        assert_eq!(source.collect::<Vec<_>>(), b"def");

        let mut source = b"ab".iter().copied();
        assert_eq!(
            super::advance_to_match(&mut source, b"abc".iter().copied()),
            0
        );
        assert_eq!(source.next(), None);
        assert_eq!(
            super::advance_to_match(&mut source, b"a".iter().copied()),
            0
        );
        assert_eq!(source.next(), None);
    }

    #[test]
    fn projection_match_consumes_successful_bytes_once() {
        let reads = std::cell::Cell::new(0usize);
        let mut source = b"   hello tail"
            .iter()
            .copied()
            .inspect(|_| reads.set(reads.get() + 1));
        assert_eq!(
            super::advance_to_match(&mut source, b"hello".iter().copied()),
            3
        );
        assert_eq!(reads.get(), 8);
        assert_eq!(source.next(), Some(b' '));
        assert_eq!(reads.get(), 9);
        assert_eq!(super::advance_to_match(&mut source, std::iter::empty()), 0);
        assert_eq!(reads.get(), 9);
        assert_eq!(source.collect::<Vec<_>>(), b"tail");
    }

    #[test]
    fn borrowed_projection_search_matches_contiguous_reference() {
        use crate::text::{Line, Span, Text, TextWrap};
        for input in [
            "",
            "   ",
            "  ab ab   ab",
            "界界 xy 界",
            "e\u{301}  e\u{301}x",
            "a\u{2003}\u{2003}b",
            // EM SPACE and EURO SIGN share their first two UTF-8 bytes.
            "a\u{2003}\u{2003}€ €",
            "       a        a",
            "a\u{2003}\u{2003}\u{2003}",
            "abcdefgh ij",
        ] {
            let spans: Vec<Span> = input
                .chars()
                .enumerate()
                .flat_map(|(index, ch)| {
                    [
                        Span::raw(""),
                        Span::styled(
                            ch.to_string(),
                            Style::new().fg(if index % 2 == 0 {
                                Color::Green
                            } else {
                                Color::Blue
                            }),
                        ),
                    ]
                })
                .collect();
            let text = Text::from_lines(vec![Line::from_spans(spans)]);
            for trim in [false, true] {
                for width in 1..=8 {
                    let block = TextBlock::new(text.clone()).wrap(TextWrap::Word).trim(trim);
                    let mut cursor = 0;
                    for row in block.projection(
                        &block.layout(Constraints::for_width(width), &mut LayoutCx::new()),
                    ) {
                        let rendered = row.line.plain_text();
                        let start = if rendered.is_empty() {
                            cursor
                        } else {
                            cursor
                                + input[cursor..]
                                    .find(&rendered)
                                    .expect("wrapped row must occur in source")
                        };
                        let end = start + rendered.len();
                        assert_eq!(
                            row.source_range,
                            start..end,
                            "input={input:?}, width={width}, trim={trim}"
                        );
                        cursor = end;
                    }
                }
            }
        }
    }

    #[test]
    fn text_projection_preserves_utf8_source_ranges_across_wraps_and_lines() {
        let component = TextBlock::new(crate::text::Text::from_lines(vec![
            crate::text::Line::raw("one  two"),
            crate::text::Line::raw("éx"),
        ]));
        let rows = component
            .projection(&component.layout(Constraints::for_width(5), &mut LayoutCx::new()));
        assert_eq!(
            rows.iter()
                .map(|row| (
                    row.line.plain_text(),
                    row.source_line,
                    row.source_range.clone()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("one  ".to_owned(), 0, 0..5),
                ("two".to_owned(), 0, 5..8),
                ("éx".to_owned(), 1, 9..12),
            ]
        );
        assert!(rows.iter().all(|row| {
            component.text.lines[row.source_line]
                .plain_text()
                .is_char_boundary(
                    row.source_range.start.saturating_sub(
                        component.text.lines[..row.source_line]
                            .iter()
                            .map(|line| line.plain_text().len() + 1)
                            .sum(),
                    ),
                )
        }));
    }

    #[test]
    fn flex_descriptor_allocates_remaining_width_without_extra_layout_node() {
        let component = Row::new()
            .child(TextBlock::new("ab"))
            .flex(Flex::new(2, TextBlock::new("flexible").id("body")));
        let layout = component.layout(Constraints::for_width(10), &mut LayoutCx::new());
        assert_eq!(layout.children.len(), 2);
        assert_eq!(layout.children[1].node.id.as_str(), "body");
        assert_eq!(layout.children[1].node.size.width, 8);
        assert_eq!(Flex::new(0, TextBlock::new("x")).weight(), 1);
    }

    #[test]
    fn keyed_wrapper_replaces_only_parent_identity() {
        let component = Keyed::new("message:42", TextBlock::new("hello").id("body"));
        let layout = component.layout(Constraints::new(0, 20, 0, None), &mut LayoutCx::new());
        assert_eq!(layout.id.as_str(), "message:42");
        assert_eq!(layout.children[0].node.id.as_str(), "body");
        assert_eq!(layout.size, layout.children[0].node.size);
        assert_ne!(
            component.revision().layout,
            Keyed::new("message:43", TextBlock::new("hello").id("body"))
                .revision()
                .layout
        );
    }

    #[test]
    fn padding_wrapper_measures_without_visual_chrome() {
        let component = Padding::new(Insets::new(1, 2, 1, 2), TextBlock::new("hello"));
        let layout = component.layout(Constraints::new(0, 20, 0, None), &mut LayoutCx::new());
        assert_eq!(layout.size.width, 9);
        assert_eq!(layout.size.height, 3);
        assert_eq!(layout.children[0].x, 2);
        assert_eq!(layout.children[0].y, 1);
    }

    #[test]
    fn fill_wrapper_paints_complete_measured_rectangle() {
        let component = Fill::new(
            Style::new().bg(Color::Blue),
            SizeBox::new(TextBlock::new("x")).width(6).height(2),
        );
        let layout = component.layout(Constraints::new(0, 10, 0, None), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(5, 1))
                .map(|cell| cell.style.bg),
            Some(Some(Color::Blue))
        );
    }

    #[test]
    fn surface_measures_padding_and_paints_complete_rectangle() {
        let component = Surface::new(TextBlock::new("hello"))
            .background(Style::new().bg(Color::Blue))
            .padding(Insets::new(1, 1, 1, 1));
        let mut layout_cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(10), &mut layout_cx);
        assert_eq!(layout.size.height, 3);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(9, 1))
                .map(|cell| cell.style.bg),
            Some(Some(Color::Blue))
        );
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some(" hello    "));
    }

    #[test]
    fn row_assigns_remaining_width_to_flexible_child() {
        let component = Row::new()
            .gap(1)
            .child(TextBlock::new("tag"))
            .flex_child(1, TextBlock::new("flexible"));
        let mut cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(12), &mut cx);

        assert_eq!(layout.children[0].node.size.width, 3);
        assert_eq!(layout.children[1].x, 4);
        assert_eq!(layout.children[1].node.size.width, 8);
        assert_eq!(layout.size.width, 12);
    }

    #[test]
    fn wrappers_constrain_align_style_clip_and_hide_children() {
        let component = Align::new(SizeBox::new(TextBlock::new("x")).width(1).height(1))
            .horizontal(HorizontalAlignment::End)
            .vertical(VerticalAlignment::End);
        let layout = component.layout(Constraints::new(5, 5, 3, Some(3)), &mut LayoutCx::new());
        assert_eq!(layout.size, crate::component::LogicalSize::new(5, 3));
        assert_eq!((layout.children[0].x, layout.children[0].y), (4, 2));

        let styled = Clip::new(StyleScope::new(
            TextBlock::new("x"),
            Style::new().bg(Color::Blue),
        ));
        let styled_layout = styled.layout(Constraints::for_width(3), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        styled.paint(&styled_layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).unwrap().style.bg,
            Some(Color::Blue)
        );

        let hidden = Visibility::new(TextBlock::new("hidden"), false);
        let hidden_layout = hidden.layout(Constraints::new(0, 8, 0, None), &mut LayoutCx::new());
        assert_eq!(hidden_layout.size.height, 0);
        assert!(hidden_layout.children.is_empty());
    }

    #[test]
    fn stack_paints_children_in_insertion_order() {
        let component = Stack::new()
            .child(Surface::new(TextBlock::new(" ")).background(Style::new().bg(Color::Blue)))
            .child(TextBlock::new("top").style(Style::new().fg(Color::White).bg(Color::Red)));
        let layout = component.layout(Constraints::for_width(4), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("top "));
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).unwrap().style.bg,
            Some(Color::Red)
        );
    }

    #[test]
    fn border_visuals_are_paint_only_while_border_sides_change_layout() {
        let plain = Surface::new(TextBlock::new("x"));
        let single = Surface::new(TextBlock::new("x")).border(crate::chrome::Border::single());
        assert_ne!(plain.revision().layout, single.revision().layout);

        let blue = Surface::new(TextBlock::new("x"))
            .border(crate::chrome::Border::single().style(Style::new().fg(Color::Blue)));
        let red = Surface::new(TextBlock::new("x"))
            .border(crate::chrome::Border::single().style(Style::new().fg(Color::Red)));
        assert_eq!(blue.revision().layout, red.revision().layout);
        assert_ne!(blue.revision().paint, red.revision().paint);

        let top_only = Surface::new(TextBlock::new("x"))
            .border(crate::chrome::Border::single().sides(crate::chrome::BorderSides::TOP));
        assert_ne!(single.revision().layout, top_only.revision().layout);
    }

    #[test]
    fn text_and_surface_configuration_revisions_separate_layout_from_paint() {
        let text_a = TextBlock::new("a");
        let text_b = TextBlock::new("b");
        assert_ne!(text_a.revision().layout, text_b.revision().layout);

        let text_plain = TextBlock::new("same");
        let text_painted = TextBlock::new(crate::text::Text::from_lines(vec![
            crate::text::Line::from_spans(vec![crate::text::Span::styled(
                "same",
                Style::new().fg(Color::Blue),
            )]),
        ]))
        .style(Style::new().bg(Color::Blue))
        .alignment(crate::text_block::Alignment::Right);
        assert_eq!(text_plain.revision().layout, text_painted.revision().layout);
        assert_ne!(text_plain.revision().paint, text_painted.revision().paint);

        let plain = Surface::new(TextBlock::new("x"));
        let painted = Surface::new(TextBlock::new("x")).background(Style::new().bg(Color::Blue));
        assert_eq!(plain.revision().layout, painted.revision().layout);
        assert_ne!(plain.revision().paint, painted.revision().paint);

        let padded = Surface::new(TextBlock::new("x")).padding(Insets::all(1));
        assert_ne!(plain.revision().layout, padded.revision().layout);
    }

    #[test]
    fn moving_spans_between_lines_invalidates_cached_layout() {
        use crate::component::LayoutId;
        use crate::text::{Line, Text};
        let first = TextBlock::new(Text::from_lines(vec![
            Line::from_spans(vec![
                crate::text::Span::raw("ab"),
                crate::text::Span::raw("cd"),
            ]),
            Line::from_spans(vec![]),
        ]));
        let second = TextBlock::new(Text::from_lines(vec![Line::raw("ab"), Line::raw("cd")]));
        let constraints = Constraints::for_width(2);
        let mut cache = crate::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        let id = LayoutId::new("text");
        assert_eq!(
            cache
                .layout(id.clone(), &first, constraints, &mut cx)
                .size
                .height,
            3
        );
        assert_eq!(
            cache.layout(id, &second, constraints, &mut cx).size.height,
            2
        );
        assert_eq!(cache.stats().misses, 2);
        assert_ne!(first.revision().layout, second.revision().layout);
    }

    #[test]
    fn descendant_layout_revision_invalidates_cached_parent() {
        let constraints = Constraints::for_width(8);
        let mut cache = crate::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        for child_revision in [1, 1, 2] {
            let component = Surface::new(RevisedLeaf(child_revision));
            cache.layout(
                crate::component::LayoutId::new("parent"),
                &component,
                constraints,
                &mut cx,
            );
        }

        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 2);
    }

    struct RevisedLeaf(u64);

    impl Component for RevisedLeaf {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                crate::component::LayoutId::new("child"),
                constraints.constrain(crate::component::LogicalSize::new(1, 1)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn revision(&self) -> ComponentRevision {
            ComponentRevision::new(self.0, 0)
        }
    }

    #[test]
    fn row_and_column_apply_cross_axis_alignment() {
        let row = Row::new()
            .alignment(VerticalAlignment::End)
            .child(SizeBox::new(TextBlock::new("one")).height(1))
            .child(SizeBox::new(TextBlock::new("two")).height(2));
        let row_layout = row.layout(Constraints::new(0, 8, 3, Some(3)), &mut LayoutCx::new());
        assert_eq!(row_layout.children[0].y, 2);
        assert_eq!(row_layout.children[1].y, 1);

        let column = Column::new()
            .alignment(HorizontalAlignment::End)
            .child(TextBlock::new("x"));
        let column_layout = column.layout(Constraints::for_width(5), &mut LayoutCx::new());
        assert_eq!(column_layout.children[0].x, 4);
        assert_eq!(column_layout.children[0].node.size.width, 1);
    }

    #[test]
    fn wrapper_options_use_the_correct_revision_channel() {
        let child = || TextBlock::new("x");

        assert_ne!(
            SizeBox::new(child()).width(2).revision().layout,
            SizeBox::new(child()).width(3).revision().layout
        );
        assert_ne!(
            Align::new(child())
                .horizontal(HorizontalAlignment::Start)
                .revision()
                .layout,
            Align::new(child())
                .horizontal(HorizontalAlignment::End)
                .revision()
                .layout
        );
        assert_ne!(
            Visibility::new(child(), true).revision().layout,
            Visibility::new(child(), false).revision().layout
        );
        assert_ne!(
            Clip::new(child()).id("first").revision().layout,
            Clip::new(child()).id("second").revision().layout
        );
        assert_ne!(
            Stack::new().id("first").child(child()).revision().layout,
            Stack::new().id("second").child(child()).revision().layout
        );

        let plain = StyleScope::new(child(), Style::new()).revision();
        let styled = StyleScope::new(child(), Style::new().fg(Color::Red)).revision();
        assert_eq!(plain.layout, styled.layout);
        assert_ne!(plain.paint, styled.paint);
    }

    #[test]
    fn row_and_column_geometry_options_change_layout_revisions() {
        let row_revision = Row::new()
            .gap(1)
            .alignment(VerticalAlignment::Center)
            .flex_child(2, TextBlock::new("x"))
            .revision();
        assert_ne!(
            row_revision,
            Row::new().child(TextBlock::new("x")).revision()
        );
        assert_ne!(
            Row::new().id("first").child(TextBlock::new("x")).revision(),
            Row::new()
                .id("second")
                .child(TextBlock::new("x"))
                .revision()
        );

        let column_revision = Column::new()
            .gap(2)
            .alignment(HorizontalAlignment::Center)
            .child(TextBlock::new("x"))
            .revision();
        assert_ne!(
            column_revision,
            Column::new().child(TextBlock::new("x")).revision()
        );
        assert_ne!(
            Column::new()
                .id("first")
                .child(TextBlock::new("x"))
                .revision(),
            Column::new()
                .id("second")
                .child(TextBlock::new("x"))
                .revision()
        );
    }

    #[test]
    fn column_places_variable_height_children_with_gap() {
        let component = Column::new()
            .gap(1)
            .child(TextBlock::new("one"))
            .child(TextBlock::new("two words wrapping"));
        let mut cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(8), &mut cx);

        assert_eq!(layout.children[0].y, 0);
        assert_eq!(layout.children[1].y, 2);
        assert_eq!(layout.size.height, 5);
    }
}
