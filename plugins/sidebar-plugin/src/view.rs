//! The sidebar's picture, built from BMUX components.
//!
//! Every row is a component: a tab card ([`CardRow`]) or a workspace header
//! ([`HeaderRow`]). They sit in a [`VirtualList`] keyed by what they show, so
//! the list measures each once per content and width, keeps the scroll
//! position on a row across changes, paints only visible rows and registers a
//! hit region for each, clipped exactly as it is painted. [`SidebarView`]
//! adds the heading above the list and the resize edge beside it. Painting
//! and hit regions therefore come from one layout; nothing is drawn over the
//! components afterwards.

use std::cell::RefCell;
use std::hash::{DefaultHasher, Hash, Hasher};

use bmux_tui::component::{
    ChildLayout, Component, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::pointer::PointerIntent;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, Text, TextWrap, TrailingAlign, TrailingPlacement};
use bmux_tui_components::virtual_list::{VirtualList, VirtualListState};

/// The list's id; its rows' hit regions are named under it.
pub const LIST_ID: &str = "sidebar.rows";
/// The id of the resize edge's hit region.
pub const RESIZE_EDGE: &str = "resize-edge";
/// The scrollbar thumb's hit region.
pub const SCROLLBAR: &str = "scrollbar";

/// Columns a row's content starts after: the active bar and a space.
const CONTENT_X: u16 = 2;
/// Columns a card's description and status start after.
const DETAIL_X: u16 = 4;
/// Rows a description wraps to at most.
const DESCRIPTION_ROWS: u64 = 2;

/// The selection identity of one text field of a row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldId {
    pub id: String,
    pub revision: u64,
}

/// How a row is painted beyond its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowChrome {
    /// The row's background, when it is tinted (active or hovered).
    pub card: Option<Style>,
    /// The bar marking the row this client shows, when it is that row.
    pub accent: Option<Style>,
    /// The style everything else is painted on.
    pub base: Style,
}

impl RowChrome {
    fn fill(self) -> Style {
        self.card.unwrap_or(self.base)
    }

    fn paint(self, cx: &mut PaintCx<'_, '_>, width: u16, height: u16) {
        if let Some(card) = self.card {
            cx.fill(LocalRect::new(0, 0, width, height), " ", card);
        }
        if let Some(accent) = self.accent {
            for row in 0..height {
                cx.set_cell(0, i64::from(row), "▎", self.fill().patch(accent));
            }
        }
    }
}

/// A tab's card: its title with the state summary at the right, then up to
/// two wrapped description rows and an optional status row.
#[derive(Debug, Clone)]
pub struct CardRow {
    pub title: Line,
    pub trailing: Option<Line>,
    pub description: String,
    pub status: String,
    pub muted: Style,
    pub chrome: RowChrome,
    pub fields: [FieldId; 3],
}

impl CardRow {
    fn detail(&self, text: &str, rows: u64, id: &str) -> Option<TextBlock> {
        let text = text.trim();
        (!text.is_empty() && rows > 0).then(|| {
            TextBlock::new(Text::from(text.to_string()))
                .id(id.to_string())
                .style(self.chrome.fill().patch(self.muted))
                .wrap(TextWrap::Word)
        })
    }

    fn description(&self) -> Option<TextBlock> {
        self.detail(&self.description, DESCRIPTION_ROWS, "description")
    }

    fn status(&self) -> Option<TextBlock> {
        self.detail(&self.status, 1, "status")
            .map(|block| block.wrap(TextWrap::None))
    }

    /// The source text of each field as painted in a row `width` cells wide:
    /// the placed title line, the description and the status. Selection
    /// fragments index into exactly these.
    pub fn field_texts(&self, width: u64) -> [String; 3] {
        [
            self.title_line(usize::try_from(content_width(width)).unwrap_or(usize::MAX))
                .plain_text(),
            self.description.trim().to_string(),
            self.status.trim().to_string(),
        ]
    }

    /// The title row's text at `width`: the summary against the right edge,
    /// kept visible by shortening the title.
    fn title_line(&self, width: usize) -> Line {
        let placed = self.trailing.as_ref().map_or_else(
            || self.title.clone(),
            |trailing| {
                bmux_tui::text::place_trailing(
                    &self.title,
                    trailing,
                    width,
                    TrailingPlacement::new(TrailingAlign::Right).keep_visible(true),
                )
            },
        );
        bmux_tui::text::truncate_line_to_display_width(&placed, width)
    }
}

/// The width a row's content may use: from the content column to one cell
/// short of the right edge (the resize edge's column on the left).
fn content_width(width: u64) -> u64 {
    width.saturating_sub(u64::from(CONTENT_X) + 1)
}

fn detail_width(width: u64) -> u64 {
    width.saturating_sub(u64::from(DETAIL_X) + 2)
}

impl Component for CardRow {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        let width = constraints.max_width();
        let mut node = LayoutNode::leaf(LayoutId::new("card"), LogicalSize::new(width, 1));
        let mut y = 1;
        for (block, rows) in [(self.description(), DESCRIPTION_ROWS), (self.status(), 1)] {
            let Some(block) = block else { continue };
            let detail = detail_width(width);
            if detail == 0 {
                continue;
            }
            let child = block.layout(Constraints::new(detail, detail, 0, Some(rows)), cx);
            let height = child.size.height;
            node.children
                .push(ChildLayout::new(u64::from(DETAIL_X), y, child));
            y += height;
        }
        node.size.height = y;
        node
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        self.chrome.paint(cx, width, height);
        let content = content_width(layout.size.width);
        let title = TextBlock::new(Text::from_lines(vec![
            self.title_line(usize::try_from(content).unwrap_or(usize::MAX)),
        ]))
        .id("title")
        .style(self.chrome.fill());
        let mut fields = vec![(
            title.clone(),
            ChildLayout::new(
                u64::from(CONTENT_X),
                0,
                title.layout(
                    Constraints::new(content, content, 1, Some(1)),
                    &mut LayoutCx::new(),
                ),
            ),
        )];
        let blocks = [self.description(), self.status()];
        for (child, block) in layout.children.iter().zip(blocks.into_iter().flatten()) {
            fields.push((block, child.clone()));
        }
        for (index, (block, child)) in fields.iter().enumerate() {
            let field = &self.fields[match child.node.id.as_str() {
                "description" => 1,
                "status" => 2,
                _ => 0,
            }];
            cx.with_child_size(
                i32::try_from(child.x).unwrap_or(i32::MAX),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                child.node.size,
                |cx| {
                    block.paint(&child.node, cx);
                    let local = Rect::new(
                        0,
                        0,
                        u16::try_from(child.node.size.width).unwrap_or(u16::MAX),
                        u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
                    );
                    cx.push_selection_scope(
                        bmux_tui::selection::SelectionScope::new(field.id.clone(), local)
                            .revision(field.revision),
                    );
                    block.register_selection(
                        &child.node,
                        cx,
                        field.id.clone(),
                        field.id.clone(),
                        u64::try_from(index).unwrap_or(u64::MAX),
                        field.revision,
                    );
                    cx.push_semantic_in(
                        LocalRect::terminal(local),
                        bmux_tui::semantic::SemanticRegion::new(
                            field.id.clone(),
                            Rect::default(),
                            "label",
                        ),
                    );
                },
            );
        }
    }
}

/// A workspace's header: its marker and name, and at the right what its
/// tabs are doing.
#[derive(Debug, Clone)]
pub struct HeaderRow {
    pub title: Line,
    pub trailing: Line,
    pub chrome: RowChrome,
}

impl Component for HeaderRow {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("header"),
            LogicalSize::new(constraints.max_width(), 1),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        self.chrome.paint(cx, width, 1);
        let content = content_width(layout.size.width);
        let content_cells = usize::try_from(content).unwrap_or(usize::MAX);
        let line = bmux_tui::text::truncate_line_to_display_width(
            &bmux_tui::text::place_trailing(
                &self.title,
                &self.trailing,
                content_cells,
                TrailingPlacement::new(TrailingAlign::Right).keep_visible(true),
            ),
            content_cells,
        );
        cx.write_line_with_fallback_style(
            LocalRect::new(
                i32::from(CONTENT_X),
                0,
                u16::try_from(content).unwrap_or(u16::MAX),
                1,
            ),
            &line,
            self.chrome.fill(),
        );
    }
}

/// One row of the list.
#[derive(Debug, Clone)]
pub enum RowView {
    Card(CardRow),
    Header(HeaderRow),
}

impl RowView {
    /// What changes the row's geometry: its text. Styles and chrome do not.
    pub fn layout_revision(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        match self {
            Self::Card(card) => {
                0_u8.hash(&mut hasher);
                card.description.hash(&mut hasher);
                card.status.hash(&mut hasher);
            }
            Self::Header(_) => 1_u8.hash(&mut hasher),
        }
        hasher.finish()
    }
}

impl Component for RowView {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        match self {
            Self::Card(card) => card.layout(constraints, cx),
            Self::Header(header) => header.layout(constraints, cx),
        }
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        match self {
            Self::Card(card) => card.paint(layout, cx),
            Self::Header(header) => header.paint(layout, cx),
        }
    }
}

/// The keyed rows, as a virtual list.
pub fn list<K>(rows: &[(K, RowView)]) -> VirtualList<'_, K>
where
    K: Clone + Ord + ToString,
{
    rows.iter()
        .fold(VirtualList::new(LIST_ID), |list, (key, row)| {
            list.item(key.clone(), row.layout_revision(), row.clone())
        })
}

/// The resize edge: its column, height, and whether it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub x: u16,
    pub height: u16,
    pub highlighted: bool,
    pub style: Style,
}

/// The whole sidebar: the heading and count, the rows below it, and the
/// resize edge.
pub struct SidebarView<'a, K> {
    pub size: (u16, u16),
    pub base: Style,
    pub heading: Line,
    pub count: Line,
    pub list: VirtualList<'a, K>,
    pub state: &'a RefCell<VirtualListState<K>>,
    /// The rows the list may use, below the heading.
    pub list_height: u16,
    /// The columns the list occupies: all of them, or all but the edge's
    /// while the rows overflow and it carries the scrollbar.
    pub list_columns: (u16, u16),
    pub scrollbar: Option<ScrollbarView>,
    pub edge: Option<Edge>,
}

/// The list's scrollbar, shown in the edge's column while the rows
/// overflow: its column, its state, and how it looks.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarView {
    pub x: u16,
    pub state: bmux_tui_components::scrollbar::ScrollbarState,
    pub styles: bmux_tui_components::scrollbar::ScrollbarStyles,
}

impl ScrollbarView {
    /// The scrollbar its view describes.
    #[must_use]
    pub const fn scrollbar(&self) -> bmux_tui_components::scrollbar::Scrollbar {
        bmux_tui_components::scrollbar::Scrollbar::new().styles(self.styles)
    }
}

impl<K> Component for SidebarView<'_, K>
where
    K: Clone + Ord + ToString,
{
    fn layout(&self, _constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("sidebar"),
            LogicalSize::new(u64::from(self.size.0), u64::from(self.size.1)),
        )
    }

    fn paint(&self, _layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let (width, height) = self.size;
        cx.fill(LocalRect::new(0, 0, width, height), " ", self.base);
        cx.write_line_with_fallback_style(
            LocalRect::new(1, 0, width.saturating_sub(1), 1),
            &self.heading,
            self.base,
        );
        let count_width = u16::try_from(self.count.width()).unwrap_or(u16::MAX);
        cx.write_line_with_fallback_style(
            LocalRect::new(
                i32::from(width.saturating_sub(1).saturating_sub(count_width)),
                0,
                count_width,
                1,
            ),
            &self.count,
            self.base,
        );
        // The list paints at its own origin; place it below the heading.
        let (list_x, list_width) = self.list_columns;
        cx.with_child_size(
            i32::from(list_x),
            1,
            LogicalSize::new(u64::from(list_width), u64::from(self.list_height)),
            |cx| {
                self.list.paint(
                    Rect::new(0, 0, list_width, self.list_height),
                    &self.state.borrow(),
                    cx,
                );
            },
        );
        // One column faces the panes: the resize edge, carrying the
        // scrollbar over the rows while they overflow.
        let track = self
            .scrollbar
            .map(|scrollbar| Rect::new(scrollbar.x, 1, 1, self.list_height));
        if let Some(edge) = self.edge {
            if edge.highlighted {
                for row in 0..edge.height {
                    let on_track =
                        track.is_some_and(|track| row >= track.y && row < track.y + track.height);
                    if !on_track {
                        cx.set_cell(i32::from(edge.x), i64::from(row), "│", edge.style);
                    }
                }
            }
            cx.push_hit(
                HitRegion::new(RESIZE_EDGE, Rect::new(edge.x, 0, 1, edge.height))
                    .role(HitRole::ResizeHandle)
                    .pointer_intent(PointerIntent::Auto),
            );
        }
        if let (Some(scrollbar), Some(track)) = (self.scrollbar, track) {
            let bar = scrollbar.scrollbar();
            bar.paint(track, &scrollbar.state, cx);
            // The thumb sits over the edge: grabbing it scrolls; the rest of
            // the column resizes.
            let layout = bar.layout(track, &scrollbar.state);
            cx.push_hit(
                HitRegion::new(
                    SCROLLBAR,
                    Rect::new(
                        track.x,
                        track.y.saturating_add(layout.thumb_start),
                        1,
                        layout.thumb_len.max(1),
                    ),
                )
                .role(HitRole::Scroll)
                .pointer_intent(PointerIntent::Auto),
            );
        }
    }
}

/// A line of `text` in `style`.
pub fn styled(text: impl Into<String>, style: Style) -> Line {
    Line::from_spans(vec![Span::styled(text.into(), style)])
}
