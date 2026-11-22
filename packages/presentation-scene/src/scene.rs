//! Scene model: where retained content is placed, what it contains beyond the
//! paint ops, and how interaction is keyed.
//!
//! Everything here is plain serializable data so the same contract can be
//! published in process or encoded across a plugin boundary.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::paint::{ExtensionRect, RenderStyle, render_text_width_u16};

/// One axis of a requested overlay size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Length {
    /// An exact number of cells, clamped to the available space.
    Cells(u16),
    /// A share of the available space, `0..=100`, at least one cell.
    Percent(u8),
    /// All of the available space.
    Fill,
}

impl Length {
    /// Resolve against `available` cells.
    #[must_use]
    pub fn resolve(self, available: u16) -> u16 {
        match self {
            Self::Cells(cells) => cells.min(available),
            Self::Percent(percent) => {
                let share = u32::from(available) * u32::from(percent.min(100)) / 100;
                u16::try_from(share)
                    .unwrap_or(available)
                    .clamp(1, available.max(1))
                    .min(available)
            }
            Self::Fill => available,
        }
    }
}

/// Requested overlay size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    pub width: Length,
    pub height: Length,
}

impl Size {
    #[must_use]
    pub const fn cells(width: u16, height: u16) -> Self {
        Self {
            width: Length::Cells(width),
            height: Length::Cells(height),
        }
    }
}

/// Space kept free between an overlay and the edges it is anchored to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Insets {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

impl Insets {
    #[must_use]
    pub const fn uniform(cells: u16) -> Self {
        Self {
            top: cells,
            right: cells,
            bottom: cells,
            left: cells,
        }
    }

    /// `rect` with these insets removed, or an empty rect when nothing is left.
    #[must_use]
    pub const fn shrink(self, rect: ExtensionRect) -> ExtensionRect {
        let w = rect.w.saturating_sub(self.left.saturating_add(self.right));
        let h = rect.h.saturating_sub(self.top.saturating_add(self.bottom));
        ExtensionRect::new(
            rect.x.saturating_add(self.left),
            rect.y.saturating_add(self.top),
            w,
            h,
        )
    }
}

/// Where in its area an overlay sits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    #[default]
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// Alignment along the edge an attached overlay sits on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

/// Which side of its target an attached overlay sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Above,
    Below,
    Left,
    Right,
}

impl Side {
    const fn opposite(self) -> Self {
        match self {
            Self::Above => Self::Below,
            Self::Below => Self::Above,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// A host-resolved overlay position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OverlayPlacement {
    /// Anchored inside the area left after layout splits.
    Anchored {
        anchor: Anchor,
        size: Size,
        margin: Insets,
    },
    /// Attached to an interactive region of one of the owner's own surfaces.
    Attached {
        /// Owner-local id of the surface holding the region.
        surface: String,
        region: HitKey,
        side: Side,
        align: Align,
        size: Size,
        /// Use the opposite side when the preferred one does not fit.
        flip: bool,
    },
}

/// Resolve an anchored overlay inside `area`.
#[must_use]
pub fn resolve_anchored(
    area: ExtensionRect,
    anchor: Anchor,
    size: Size,
    margin: Insets,
) -> Option<ExtensionRect> {
    let inner = margin.shrink(area);
    if inner.w == 0 || inner.h == 0 {
        return None;
    }
    let w = size.width.resolve(inner.w);
    let h = size.height.resolve(inner.h);
    if w == 0 || h == 0 {
        return None;
    }
    let free_x = inner.w - w;
    let free_y = inner.h - h;
    let (x, y) = match anchor {
        Anchor::TopLeft => (0, 0),
        Anchor::Top => (free_x / 2, 0),
        Anchor::TopRight => (free_x, 0),
        Anchor::Left => (0, free_y / 2),
        Anchor::Center => (free_x / 2, free_y / 2),
        Anchor::Right => (free_x, free_y / 2),
        Anchor::BottomLeft => (0, free_y),
        Anchor::Bottom => (free_x / 2, free_y),
        Anchor::BottomRight => (free_x, free_y),
    };
    Some(ExtensionRect::new(inner.x + x, inner.y + y, w, h))
}

/// Resolve an overlay attached to `target` (absolute), kept inside `bounds`.
///
/// The overlay is sized against the space on its side; when `flip` is set and
/// the preferred side cannot hold the requested size, the opposite side is used
/// if it has more room. The cross axis is aligned to the target and then
/// clamped into `bounds`.
#[must_use]
pub fn resolve_attached(
    bounds: ExtensionRect,
    target: ExtensionRect,
    side: Side,
    align: Align,
    dimensions: Size,
    flip: bool,
) -> Option<ExtensionRect> {
    let room = |side: Side| -> u16 {
        match side {
            Side::Above => target.y.saturating_sub(bounds.y),
            Side::Below => bounds.bottom().saturating_sub(target.bottom()),
            Side::Left => target.x.saturating_sub(bounds.x),
            Side::Right => bounds.right().saturating_sub(target.right()),
        }
    };
    let wanted = |side: Side| -> u16 {
        match side {
            Side::Above | Side::Below => dimensions.height.resolve(bounds.h),
            Side::Left | Side::Right => dimensions.width.resolve(bounds.w),
        }
    };
    let side = if flip && room(side) < wanted(side) && room(side.opposite()) > room(side) {
        side.opposite()
    } else {
        side
    };
    let along = room(side);
    let vertical = matches!(side, Side::Above | Side::Below);
    let (main, cross_available) = if vertical {
        (dimensions.height.resolve(along), bounds.w)
    } else {
        (dimensions.width.resolve(along), bounds.h)
    };
    let cross = if vertical {
        dimensions.width.resolve(cross_available)
    } else {
        dimensions.height.resolve(cross_available)
    };
    if main == 0 || cross == 0 {
        return None;
    }
    let (target_start, target_len, bounds_start, bounds_len) = if vertical {
        (target.x, target.w, bounds.x, bounds.w)
    } else {
        (target.y, target.h, bounds.y, bounds.h)
    };
    let start = match align {
        Align::Start => target_start,
        Align::Center => {
            let center = target_start.saturating_add(target_len / 2);
            center.saturating_sub(cross / 2)
        }
        Align::End => target_start
            .saturating_add(target_len)
            .saturating_sub(cross),
    };
    let max_start = bounds_start.saturating_add(bounds_len.saturating_sub(cross));
    let start = start.clamp(bounds_start, max_start);
    let rect = match side {
        Side::Above => ExtensionRect::new(start, target.y - main, cross, main),
        Side::Below => ExtensionRect::new(start, target.bottom(), cross, main),
        Side::Left => ExtensionRect::new(target.x - main, start, main, cross),
        Side::Right => ExtensionRect::new(target.right(), start, main, cross),
    };
    Some(rect)
}

/// Where a pane-attached scene paints relative to the pane's own content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaneDepth {
    /// Shows only through transparent pane cells and keeps the pane background.
    UnderContent,
    /// Paints over the pane's content.
    OverContent,
}

/// A displayed glyph: one scalar or one extended grapheme cluster.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SceneSymbol {
    Char(char),
    Cluster(Box<str>),
}

impl SceneSymbol {
    /// Width in terminal cells.
    #[must_use]
    pub fn width(&self) -> u16 {
        match self {
            Self::Char(ch) => {
                let mut buffer = [0; 4];
                render_text_width_u16(ch.encode_utf8(&mut buffer))
            }
            Self::Cluster(cluster) => render_text_width_u16(cluster),
        }
    }

    /// The glyph's text.
    #[must_use]
    pub fn as_str<'a>(&'a self, buffer: &'a mut [u8; 4]) -> &'a str {
        match self {
            Self::Char(ch) => ch.encode_utf8(buffer),
            Self::Cluster(cluster) => cluster,
        }
    }
}

/// One cell of a [`CellBlock`]. `symbol: None` is transparent: the cell keeps
/// whatever is beneath it. A wide symbol covers the cells after it; those
/// cells must be transparent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SceneCell {
    pub symbol: Option<SceneSymbol>,
    pub style: RenderStyle,
}

impl SceneCell {
    #[must_use]
    pub const fn transparent() -> Self {
        Self {
            symbol: None,
            style: RenderStyle::new(),
        }
    }

    #[must_use]
    pub const fn new(ch: char, style: RenderStyle) -> Self {
        Self {
            symbol: Some(SceneSymbol::Char(ch)),
            style,
        }
    }

    #[must_use]
    pub fn cluster(cluster: impl Into<Box<str>>, style: RenderStyle) -> Self {
        Self {
            symbol: Some(SceneSymbol::Cluster(cluster.into())),
            style,
        }
    }
}

/// A rectangular block of grapheme-aware cells with per-cell transparency.
///
/// This is the cell model shared by retained presentations: a superset of
/// sparse `RenderCell` grids and opaque component buffers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellBlock {
    /// Top-left corner in surface-local cells.
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    /// Row-major, `width * height` entries.
    pub cells: Vec<SceneCell>,
}

impl CellBlock {
    /// A transparent block.
    #[must_use]
    pub fn transparent(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
            cells: vec![SceneCell::transparent(); usize::from(width) * usize::from(height)],
        }
    }

    /// The cell at block-local `(column, row)`.
    #[must_use]
    pub fn get(&self, column: u16, row: u16) -> Option<&SceneCell> {
        if column >= self.width || row >= self.height {
            return None;
        }
        self.cells
            .get(usize::from(row) * usize::from(self.width) + usize::from(column))
    }

    /// Replace the cell at block-local `(column, row)`.
    pub fn set(&mut self, column: u16, row: u16, cell: SceneCell) {
        if column < self.width && row < self.height {
            let index = usize::from(row) * usize::from(self.width) + usize::from(column);
            if let Some(slot) = self.cells.get_mut(index) {
                *slot = cell;
            }
        }
    }

    /// Whether the cell count matches the declared size.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.cells.len() == usize::from(self.width) * usize::from(self.height)
    }

    /// Bounds in surface-local cells.
    #[must_use]
    pub const fn rect(&self) -> ExtensionRect {
        ExtensionRect::new(self.x, self.y, self.width, self.height)
    }
}

/// Wire identity of an interactive region within one surface revision.
///
/// Producers keep the typed meaning on their side in a [`HitTable`]; the host
/// only routes the integer back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HitKey(pub u64);

/// Producer-side typed meaning of the [`HitKey`]s one revision publishes.
#[derive(Debug, Clone)]
pub struct HitTable<K> {
    keys: Vec<K>,
}

impl<K> Default for HitTable<K> {
    fn default() -> Self {
        Self { keys: Vec::new() }
    }
}

impl<K: PartialEq> HitTable<K> {
    #[must_use]
    pub const fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// The wire key for `key`, allocating one when it is new in this table.
    pub fn key(&mut self, key: K) -> HitKey {
        let index = self
            .keys
            .iter()
            .position(|existing| *existing == key)
            .unwrap_or_else(|| {
                self.keys.push(key);
                self.keys.len() - 1
            });
        HitKey(index as u64)
    }

    /// The typed meaning of a wire key.
    #[must_use]
    pub fn get(&self, key: HitKey) -> Option<&K> {
        usize::try_from(key.0)
            .ok()
            .and_then(|index| self.keys.get(index))
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.keys.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Hit tables for the last few published revisions of one surface.
///
/// Input addressed to a revision that is still on screen resolves; input
/// addressed to a retired revision is rejected instead of misrouted.
#[derive(Debug, Clone)]
pub struct HitTables<K> {
    retained: usize,
    tables: BTreeMap<u64, HitTable<K>>,
}

impl<K: PartialEq> HitTables<K> {
    /// Keep the tables of the newest `retained` revisions (at least one).
    #[must_use]
    pub const fn new(retained: usize) -> Self {
        Self {
            retained: if retained == 0 { 1 } else { retained },
            tables: BTreeMap::new(),
        }
    }

    /// Record the table published with `revision`.
    pub fn publish(&mut self, revision: u64, table: HitTable<K>) {
        self.tables.insert(revision, table);
        while self.tables.len() > self.retained {
            self.tables.pop_first();
        }
    }

    /// Resolve `key` as published in `revision`.
    #[must_use]
    pub fn resolve(&self, revision: u64, key: HitKey) -> Option<&K> {
        self.tables.get(&revision).and_then(|table| table.get(key))
    }

    /// Forget every revision.
    pub fn clear(&mut self) {
        self.tables.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_clamp_to_the_available_space() {
        assert_eq!(Length::Cells(10).resolve(4), 4);
        assert_eq!(Length::Percent(50).resolve(9), 4);
        assert_eq!(Length::Percent(1).resolve(9), 1);
        assert_eq!(Length::Percent(200).resolve(9), 9);
        assert_eq!(Length::Fill.resolve(7), 7);
        assert_eq!(Length::Percent(50).resolve(0), 0);
    }

    #[test]
    fn anchored_overlays_sit_in_their_corner_inside_the_margin() {
        let area = ExtensionRect::new(10, 0, 30, 10);
        let size = Size::cells(8, 2);
        let margin = Insets::uniform(1);
        assert_eq!(
            resolve_anchored(area, Anchor::BottomRight, size, margin),
            Some(ExtensionRect::new(31, 7, 8, 2))
        );
        assert_eq!(
            resolve_anchored(area, Anchor::TopLeft, size, margin),
            Some(ExtensionRect::new(11, 1, 8, 2))
        );
        assert_eq!(
            resolve_anchored(area, Anchor::Center, size, Insets::default()),
            Some(ExtensionRect::new(21, 4, 8, 2))
        );
        // Larger than the area: clamped.
        assert_eq!(
            resolve_anchored(area, Anchor::Center, Size::cells(99, 99), margin),
            Some(ExtensionRect::new(11, 1, 28, 8))
        );
        assert_eq!(
            resolve_anchored(ExtensionRect::new(0, 0, 2, 2), Anchor::Center, size, margin),
            None
        );
    }

    #[test]
    fn attached_overlays_follow_their_target_and_flip_when_needed() {
        let bounds = ExtensionRect::new(0, 0, 40, 12);
        // A tab cell on the bottom row: a menu below has no room, so it flips above.
        let tab = ExtensionRect::new(30, 11, 6, 1);
        let menu = resolve_attached(
            bounds,
            tab,
            Side::Below,
            Align::Start,
            Size::cells(12, 4),
            true,
        );
        assert_eq!(menu, Some(ExtensionRect::new(28, 7, 12, 4)));
        // Without flip it is squeezed into the room below, which is none.
        assert_eq!(
            resolve_attached(
                bounds,
                tab,
                Side::Below,
                Align::Start,
                Size::cells(12, 4),
                false
            ),
            None
        );
        // A tab at the top: below fits, aligned to the tab's start.
        let top = ExtensionRect::new(3, 0, 6, 1);
        assert_eq!(
            resolve_attached(
                bounds,
                top,
                Side::Below,
                Align::Start,
                Size::cells(12, 4),
                true
            ),
            Some(ExtensionRect::new(3, 1, 12, 4))
        );
        // Centered and to the right of a sidebar row.
        let row = ExtensionRect::new(0, 5, 14, 1);
        assert_eq!(
            resolve_attached(
                bounds,
                row,
                Side::Right,
                Align::Center,
                Size::cells(10, 3),
                true
            ),
            Some(ExtensionRect::new(14, 4, 10, 3))
        );
    }

    #[test]
    fn hit_tables_resolve_only_retained_revisions() {
        #[derive(Debug, PartialEq)]
        enum Hit {
            Tab(u32),
            Close,
        }
        let mut first = HitTable::new();
        let tab = first.key(Hit::Tab(7));
        let close = first.key(Hit::Close);
        assert_eq!(first.key(Hit::Tab(7)), tab);
        let mut tables = HitTables::new(2);
        tables.publish(1, first);
        assert_eq!(tables.resolve(1, tab), Some(&Hit::Tab(7)));
        assert_eq!(tables.resolve(1, close), Some(&Hit::Close));
        assert_eq!(tables.resolve(2, tab), None);
        let mut second = HitTable::new();
        second.key(Hit::Close);
        tables.publish(2, second);
        tables.publish(3, HitTable::new());
        assert_eq!(
            tables.resolve(1, tab),
            None,
            "retired revisions are rejected"
        );
        assert_eq!(tables.resolve(2, HitKey(0)), Some(&Hit::Close));
    }

    #[test]
    fn cell_blocks_address_cells_and_report_wide_symbols() {
        let mut block = CellBlock::transparent(1, 2, 3, 2);
        assert!(block.is_well_formed());
        block.set(0, 1, SceneCell::cluster("👍🏽", RenderStyle::new()));
        assert_eq!(
            block
                .get(0, 1)
                .and_then(|cell| cell.symbol.as_ref())
                .map(SceneSymbol::width),
            Some(2)
        );
        assert_eq!(block.get(3, 0), None);
        assert_eq!(block.rect(), ExtensionRect::new(1, 2, 3, 2));
    }
}
