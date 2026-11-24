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
use bmux_tui_components::panel_window::WindowControls;
use bmux_tui_components::virtual_list::{VirtualList, VirtualListState};

/// The list's id; its rows' hit regions are named under it.
pub const LIST_ID: &str = "sidebar.rows";
/// The id of the resize edge's hit region.
pub const RESIZE_EDGE: &str = "resize-edge";
/// The scrollbar thumb's hit region.
pub const SCROLLBAR: &str = "scrollbar";
pub const WINDOW_ID: &str = "sidebar.window";

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
    /// The bar down the row's first column: thick for the row this client
    /// shows, a thin rail in its workspace's color under a workspace header.
    pub bar: Option<(&'static str, Style)>,
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
        if let Some((glyph, style)) = self.bar {
            for row in 0..height {
                cx.set_cell(0, i64::from(row), glyph, self.fill().patch(style));
            }
        }
    }
}

/// What a card shows below its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detail {
    /// Nothing: a one-line card.
    None,
    /// One line, cut to fit, such as the directory and git status.
    Line(Line),
    /// Text wrapped to at most [`DESCRIPTION_ROWS`] rows, such as what an
    /// agent asks.
    Wrapped(Line),
}

/// A tab's card: its title with the state summary at the right, then its
/// detail and an optional status row.
#[derive(Debug, Clone)]
pub struct CardRow {
    pub title: Line,
    pub trailing: Option<Line>,
    pub description: Detail,
    pub status: String,
    pub muted: Style,
    pub chrome: RowChrome,
    pub fields: [FieldId; 3],
    /// A blank row below the card, setting its group off from the next.
    pub spacer: bool,
}

impl CardRow {
    fn block(&self, line: Line, id: &str, wrap: TextWrap) -> Option<TextBlock> {
        (!line.plain_text().trim().is_empty()).then(|| {
            TextBlock::new(Text::from_lines(vec![line]))
                .id(id.to_string())
                .style(self.chrome.fill().patch(self.muted))
                .wrap(wrap)
        })
    }

    /// The detail as laid out `width` cells wide: one line is cut to fit,
    /// keeping the cut visible.
    fn description(&self, width: u64) -> Option<TextBlock> {
        match &self.description {
            Detail::None => None,
            Detail::Line(line) => self.block(
                bmux_tui::text::truncate_line_to_display_width(
                    line,
                    usize::try_from(width).unwrap_or(usize::MAX),
                ),
                "description",
                TextWrap::None,
            ),
            Detail::Wrapped(line) => self.block(line.clone(), "description", TextWrap::Word),
        }
    }

    const fn description_rows(&self) -> u64 {
        match self.description {
            Detail::Wrapped(_) => DESCRIPTION_ROWS,
            _ => 1,
        }
    }

    fn status(&self) -> Option<TextBlock> {
        self.block(
            Line::from(self.status.trim().to_string()),
            "status",
            TextWrap::None,
        )
    }

    /// The source text of each field as painted in a row `width` cells wide:
    /// the placed title line, the description and the status. Selection
    /// fragments index into exactly these.
    pub fn field_texts(&self, width: u64) -> [String; 3] {
        [
            self.title_line(usize::try_from(content_width(width)).unwrap_or(usize::MAX))
                .plain_text(),
            self.description(detail_width(width))
                .map(|block| {
                    block
                        .text()
                        .lines
                        .iter()
                        .map(Line::plain_text)
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim()
                        .to_string()
                })
                .unwrap_or_default(),
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
        let detail = detail_width(width);
        for (block, rows) in [
            (self.description(detail), self.description_rows()),
            (self.status(), 1),
        ] {
            let Some(block) = block else { continue };
            if detail == 0 {
                continue;
            }
            let child = block.layout(Constraints::new(detail, detail, 0, Some(rows)), cx);
            let height = child.size.height;
            node.children
                .push(ChildLayout::new(u64::from(DETAIL_X), y, child));
            y += height;
        }
        node.size.height = y + u64::from(self.spacer);
        node
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height)
            .unwrap_or(u16::MAX)
            .saturating_sub(u16::from(self.spacer));
        self.chrome.paint(cx, width, height);
        if self.spacer {
            cx.fill(
                LocalRect::new(0, i64::from(height), width, 1),
                " ",
                self.chrome.base,
            );
        }
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
        let blocks = [
            self.description(detail_width(layout.size.width)),
            self.status(),
        ];
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

/// A workspace's header: a band across the sidebar with the workspace's
/// color, its name, and at the right what its tabs are doing and whether it
/// is collapsed. It is a sticky section header: while its tabs scroll under
/// the top, it stays pinned there.
#[derive(Debug, Clone)]
pub struct HeaderRow {
    pub title: Line,
    pub trailing: Line,
    /// The band's background.
    pub band: Style,
    /// The swatch at the band's start, in the workspace's color.
    pub swatch: Option<Style>,
    /// A blank row below a collapsed header, setting it off from the next.
    pub spacer: bool,
    pub base: Style,
}

impl Component for HeaderRow {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            LayoutId::new("header"),
            LogicalSize::new(constraints.max_width(), 1 + u64::from(self.spacer)),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let y = 0;
        cx.fill(LocalRect::new(0, y, width, 1), " ", self.band);
        if self.spacer {
            cx.fill(LocalRect::new(0, 1, width, 1), " ", self.base);
        }
        if let Some(swatch) = self.swatch {
            cx.set_cell(0, y, "▌", self.band.patch(swatch));
        }
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
                y,
                u16::try_from(content).unwrap_or(u16::MAX),
                1,
            ),
            &line,
            self.band,
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
                match &card.description {
                    Detail::None => 0_u8.hash(&mut hasher),
                    Detail::Line(line) => {
                        1_u8.hash(&mut hasher);
                        line.plain_text().hash(&mut hasher);
                    }
                    Detail::Wrapped(line) => {
                        2_u8.hash(&mut hasher);
                        line.plain_text().hash(&mut hasher);
                    }
                }
                card.status.hash(&mut hasher);
                card.spacer.hash(&mut hasher);
            }
            Self::Header(header) => {
                1_u8.hash(&mut hasher);
                header.spacer.hash(&mut hasher);
            }
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
        .fold(VirtualList::new(LIST_ID), |list, (key, row)| match row {
            RowView::Header(_) => {
                list.section_header(key.clone(), 0, row.layout_revision(), row.clone())
            }
            RowView::Card(_) => list.item(key.clone(), row.layout_revision(), row.clone()),
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
    pub controls: WindowControls,
    pub strip_summary: Option<Vec<Line>>,
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
/// overflow: its column, its state, its glyphs, and how it looks.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarView {
    pub x: u16,
    pub state: bmux_tui_components::scrollbar::ScrollbarState,
    pub policy: bmux_tui_components::scrollbar::ScrollbarPolicy,
    pub styles: bmux_tui_components::scrollbar::ScrollbarStyles,
}

impl ScrollbarView {
    /// The scrollbar its view describes.
    #[must_use]
    pub const fn scrollbar(&self) -> bmux_tui_components::scrollbar::Scrollbar {
        bmux_tui_components::scrollbar::Scrollbar::new()
            .policy(self.policy)
            .styles(self.styles)
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
        self.paint_header(cx);
        if let Some(summary) = &self.strip_summary {
            let x = u16::from(self.edge.is_some_and(|edge| edge.x == 0));
            let room = width.saturating_sub(u16::from(self.edge.is_some()));
            for (y, line) in (1..height).zip(summary) {
                cx.write_line_with_fallback_style(
                    LocalRect::new(i32::from(x), i64::from(y), room, 1),
                    line,
                    self.base,
                );
            }
        } else {
            self.paint_list(cx);
        }
        self.paint_edge(cx);
    }
}

impl<K: Clone + Ord + ToString> SidebarView<'_, K> {
    fn paint_header(&self, cx: &mut PaintCx<'_, '_>) {
        let width = self.size.0;
        let left = self.edge.is_some_and(|edge| edge.x == 0);
        let start = u16::from(left);
        let controls = self.controls.paint_inline(
            WINDOW_ID,
            Rect::new(
                start,
                0,
                width.saturating_sub(u16::from(self.edge.is_some())),
                1,
            ),
            cx,
        );
        if self.strip_summary.is_some() {
            return;
        }
        let end = controls.saturating_sub(1);
        let heading_start = start.max(1);
        let count_width = u16::try_from(self.count.width()).unwrap_or(u16::MAX);
        let count_start = end.saturating_sub(count_width);
        let title_end = if count_width == 0 {
            end
        } else {
            count_start.saturating_sub(1)
        };
        cx.write_line_with_fallback_style(
            LocalRect::new(
                i32::from(heading_start),
                0,
                title_end.saturating_sub(heading_start),
                1,
            ),
            &self.heading,
            self.base,
        );
        cx.write_line_with_fallback_style(
            LocalRect::new(i32::from(count_start), 0, count_width, 1),
            &self.count,
            self.base,
        );
    }

    fn paint_list(&self, cx: &mut PaintCx<'_, '_>) {
        // The list paints at its own origin; place it below the heading.
        let (list_x, list_width) = self.list_columns;
        cx.with_child_size(
            i32::from(list_x),
            1,
            LogicalSize::new(u64::from(list_width), u64::from(self.list_height)),
            |cx| {
                // The list's own component pins each group's header at the
                // top while its tabs scroll under it; the sidebar draws its
                // own scrollbar and handles its own input.
                let list = bmux_tui_components::virtual_list::VirtualListComponent::new(
                    LIST_ID, &self.list, self.state,
                )
                .chrome(bmux_tui_components::virtual_list::VirtualListChrome::bare());
                let node = list.layout(
                    Constraints::tight(bmux_tui::geometry::Size::new(list_width, self.list_height)),
                    &mut LayoutCx::new(),
                );
                list.paint(&node, cx);
            },
        );
    }

    fn paint_edge(&self, cx: &mut PaintCx<'_, '_>) {
        // One column faces the panes: the resize edge, carrying the
        // scrollbar over the rows while they overflow.
        let track = self
            .scrollbar
            .map(|scrollbar| Rect::new(scrollbar.x, 1, 1, self.list_height));
        if let Some(edge) = self.edge {
            if edge.highlighted || self.strip_summary.is_some() {
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
