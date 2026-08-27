//! Visible-only rendering for keyed variable-height component collections.

use std::collections::BTreeSet;

use bmux_tui::component::{Component, Constraints, Element, LayoutCache, LayoutCx, LayoutId};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::measured_list::MeasuredListIndex;
use bmux_tui::paint::{LocalRect, PaintCx};

use crate::scroll_view::ScrollViewState;

/// Structural work counters for one virtual-list render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VirtualListRenderStats {
    /// Items whose components were painted.
    pub painted_items: usize,
    /// Visible item interaction regions registered.
    pub registered_items: usize,
}

/// Retained state for keyed variable-height collection layout.
#[derive(Debug)]
pub struct VirtualListState<K> {
    index: MeasuredListIndex<K>,
    layouts: LayoutCache,
    anchor: Option<(K, usize, usize)>,
    /// Shared logical scroll state.
    pub scroll: ScrollViewState,
}

impl<K> VirtualListState<K>
where
    K: Clone + Ord,
{
    /// Create empty retained state with one logical inter-item gap.
    #[must_use]
    pub fn new(gap: usize) -> Self {
        Self {
            index: MeasuredListIndex::new(gap),
            layouts: LayoutCache::new(),
            anchor: None,
            scroll: ScrollViewState::new(),
        }
    }

    /// Capture the current top item and row within it as a stable mutation and
    /// reflow anchor.
    pub fn capture_anchor(&mut self) {
        let offset = self.scroll.vertical_offset();
        self.anchor = self.index.item_at_offset(offset).and_then(|index| {
            let item = self.index.item(index)?;
            let start = self.index.item_offset(index)?;
            Some((item.key.clone(), index, offset.saturating_sub(start)))
        });
    }

    /// Restore the captured stable-key anchor after synchronization.
    pub fn restore_anchor(&mut self, viewport_height: usize) {
        if self.scroll.follows_bottom() {
            let maximum = self.index.total_height().saturating_sub(viewport_height);
            self.scroll.set_vertical_offset(maximum);
            self.scroll.set_follow_bottom(true);
            return;
        }
        let Some((key, former_index, row)) = self.anchor.as_ref() else {
            self.clamp_scroll(viewport_height);
            return;
        };
        let index = self.index.index_of(key).or_else(|| {
            (!self.index.is_empty()).then(|| (*former_index).min(self.index.len() - 1))
        });
        let Some(index) = index else {
            self.scroll.set_vertical_offset(0);
            self.anchor = None;
            return;
        };
        let start = self.index.item_offset(index).unwrap_or(0);
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll
            .set_vertical_offset(start.saturating_add(*row).min(maximum));
        self.capture_anchor();
    }

    /// Clamp logical scrolling to the current collection extent.
    pub fn clamp_scroll(&mut self, viewport_height: usize) {
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll
            .set_vertical_offset(self.scroll.vertical_offset().min(maximum));
    }

    /// Retained measured-item index.
    #[must_use]
    pub const fn index(&self) -> &MeasuredListIndex<K> {
        &self.index
    }

    /// Retained layout cache diagnostics.
    #[must_use]
    pub const fn layout_cache(&self) -> &LayoutCache {
        &self.layouts
    }

    /// Mutable retained measured-item index.
    pub const fn index_mut(&mut self) -> &mut MeasuredListIndex<K> {
        &mut self.index
    }
}

/// A keyed item supplied to [`VirtualList`].
pub struct VirtualListItem<'a, K> {
    key: K,
    layout_revision: u64,
    component: Element<'a>,
}

impl<'a, K> VirtualListItem<'a, K> {
    /// Create one keyed item component.
    #[must_use]
    pub fn new(key: K, layout_revision: u64, component: impl Component + 'a) -> Self {
        Self {
            key,
            layout_revision,
            component: Element::new(component),
        }
    }
}

/// A variable-height collection that measures by stable key and paints only
/// viewport-intersecting items.
pub struct VirtualList<'a, K> {
    id: String,
    items: Vec<VirtualListItem<'a, K>>,
}

impl<'a, K> VirtualList<'a, K>
where
    K: Clone + Ord + ToString,
{
    /// Create an empty collection with a stable semantic identifier prefix.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            items: Vec::new(),
        }
    }

    /// Append one keyed item.
    #[must_use]
    pub fn item(mut self, key: K, layout_revision: u64, component: impl Component + 'a) -> Self {
        self.items
            .push(VirtualListItem::new(key, layout_revision, component));
        self
    }

    /// Synchronize exact current-width measurements, retaining unchanged keyed
    /// layouts across reorder.
    ///
    /// # Panics
    ///
    /// Panics if duplicate stable item keys are supplied.
    pub fn sync(&self, width: u16, state: &mut VirtualListState<K>, cx: &mut LayoutCx) {
        let by_key = self
            .items
            .iter()
            .map(|item| (item.key.clone(), item))
            .collect::<std::collections::BTreeMap<_, _>>();
        state.index.sync(
            self.items
                .iter()
                .map(|item| (item.key.clone(), item.layout_revision)),
            width,
            |key| {
                let item = by_key.get(key).expect("synchronized key must exist");
                state
                    .layouts
                    .layout_with_revision(
                        item_layout_id(&self.id, key),
                        item.component.as_component(),
                        item.layout_revision,
                        Constraints::for_width(width),
                        cx,
                    )
                    .size
                    .height
            },
        );
        let active = self
            .items
            .iter()
            .map(|item| item_layout_id(&self.id, &item.key))
            .collect::<BTreeSet<_>>();
        state.layouts.retain_ids(&active);
    }

    /// Paint only visible items and register full visible item hit rectangles.
    ///
    /// # Panics
    ///
    /// Panics if called before synchronizing the list at `area.width`.
    pub fn paint(
        &self,
        area: Rect,
        state: &VirtualListState<K>,
        cx: &mut PaintCx<'_, '_>,
    ) -> VirtualListRenderStats {
        let offset = state.scroll.vertical_offset();
        let range = state.index.visible_range(offset, usize::from(area.height));
        let mut report = VirtualListRenderStats::default();
        for index in range.start..range.end {
            let Some(item) = self.items.get(index) else {
                continue;
            };
            let Some(measured) = state.index.item(index) else {
                continue;
            };
            let Some(start) = state.index.item_offset(index) else {
                continue;
            };
            let layout_id = item_layout_id(&self.id, &item.key);
            let constraints = Constraints::for_width(area.width);
            let layout = state
                .layouts
                .get(&layout_id, item.layout_revision, constraints)
                .expect("visible synchronized item must have retained layout");
            debug_assert_eq!(layout.size.height, measured.height);
            let local_y = i64::try_from(start)
                .unwrap_or(i64::MAX)
                .saturating_sub(i64::try_from(offset).unwrap_or(i64::MAX));
            let item_height = u16::try_from(measured.height).unwrap_or(u16::MAX);
            cx.with_child(
                0,
                local_y,
                LocalRect::new(0, -local_y, area.width, area.height),
                |cx| {
                    item.component.paint(layout, cx);
                    cx.push_hit(
                        HitRegion::new(
                            format!("{}.item.{}", self.id, item.key.to_string()),
                            Rect::new(0, 0, area.width, item_height),
                        )
                        .role(HitRole::ListItem),
                    );
                },
            );
            report.painted_items = report.painted_items.saturating_add(1);
            report.registered_items = report.registered_items.saturating_add(1);
        }
        report
    }

    /// Scroll so the keyed item is visible with the minimum movement.
    pub fn ensure_item_visible(
        &self,
        state: &mut VirtualListState<K>,
        key: &K,
        viewport_height: usize,
    ) -> bool {
        let Some(index) = state.index.index_of(key) else {
            return false;
        };
        let Some(start) = state.index.item_offset(index) else {
            return false;
        };
        let height = state.index.item(index).map_or(0, |item| item.height);
        let offset = state.scroll.vertical_offset();
        let end = start.saturating_add(height);
        let next = if start < offset {
            start
        } else if end > offset.saturating_add(viewport_height) {
            end.saturating_sub(viewport_height)
        } else {
            offset
        };
        state.scroll.set_vertical_offset(next);
        state.clamp_scroll(viewport_height);
        true
    }

    /// Logical start row for a stable key.
    #[must_use]
    pub fn item_offset(&self, state: &VirtualListState<K>, key: &K) -> Option<usize> {
        state
            .index
            .index_of(key)
            .and_then(|index| state.index.item_offset(index))
    }
}

impl<K> Default for VirtualListState<K>
where
    K: Clone + Ord,
{
    fn default() -> Self {
        Self::new(0)
    }
}

fn item_layout_id<K: ToString>(list_id: &str, key: &K) -> LayoutId {
    LayoutId::new(format!("{list_id}.item.{}", key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{VirtualList, VirtualListState};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{
        Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize,
    };
    use bmux_tui::composition::TextContent;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;

    struct ExternallyRevisedItem;

    impl Component for ExternallyRevisedItem {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new("external-item"),
                constraints.constrain(LogicalSize::new(constraints.max_width(), 1)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn revision(&self) -> ComponentRevision {
            ComponentRevision::default()
        }
    }

    #[test]
    fn viewport_height_does_not_remeasure_but_width_and_removed_keys_do() {
        let list = VirtualList::new("messages")
            .item("a", 1, TextContent::new("a message that wraps"))
            .item("b", 1, TextContent::new("b"));
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        list.sync(8, &mut state, &mut cx);
        let initial_measurements = cx.measured_nodes();

        for viewport_height in [1, 3, 20] {
            state.capture_anchor();
            state.restore_anchor(viewport_height);
            list.sync(8, &mut state, &mut cx);
        }
        assert_eq!(cx.measured_nodes(), initial_measurements);

        let reordered = VirtualList::new("messages")
            .item("b", 1, TextContent::new("b"))
            .item("a", 1, TextContent::new("a message that wraps"));
        reordered.sync(8, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), initial_measurements);

        reordered.sync(6, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), initial_measurements + 2);
        let retained_before_removal = state.layout_cache().len();

        VirtualList::new("messages")
            .item("a", 1, TextContent::new("a message that wraps"))
            .sync(6, &mut state, &mut cx);
        assert!(state.layout_cache().len() < retained_before_removal);
        assert!(state.layout_cache().stats().released > 0);
    }

    #[test]
    fn external_item_revision_invalidates_retained_layout() {
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        VirtualList::new("messages")
            .item("a", 1, ExternallyRevisedItem)
            .sync(8, &mut state, &mut cx);
        VirtualList::new("messages")
            .item("a", 2, ExternallyRevisedItem)
            .sync(8, &mut state, &mut cx);

        assert_eq!(cx.measured_nodes(), 2);
    }

    #[test]
    fn paints_and_registers_only_intersecting_variable_height_items() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b line that wraps"))
            .item("c", 0, TextContent::new("c"))
            .item("d", 0, TextContent::new("d"));
        let mut state = VirtualListState::new(1);
        list.sync(6, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 3));
        let mut frame = Frame::new(&mut buffer);
        let report = list.paint(Rect::new(0, 0, 6, 3), &state, &mut PaintCx::new(&mut frame));

        assert_eq!(report.painted_items, 1);
        assert_eq!(frame.hits().regions().len(), 1);
        assert_eq!(frame.hits().regions()[0].id.as_str(), "messages.item.b");
    }

    #[test]
    fn removed_anchor_falls_forward_then_back_and_empty_resets() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b"))
            .item("c", 0, TextContent::new("c"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state
            .scroll
            .set_vertical_offset(initial.item_offset(&state, &"b").unwrap());
        state.capture_anchor();

        let removed = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("c", 0, TextContent::new("c"));
        removed.sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(1);
        assert_eq!(state.scroll.vertical_offset(), 2);

        let empty: VirtualList<'_, &str> = VirtualList::new("messages");
        empty.sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(1);
        assert_eq!(state.scroll.vertical_offset(), 0);
    }

    #[test]
    fn stable_top_anchor_survives_insertion_and_width_reflow() {
        let first = VirtualList::new("messages")
            .item("a", 0, TextContent::new("short"))
            .item("b", 0, TextContent::new("b message wraps here"))
            .item("c", 0, TextContent::new("c"));
        let mut state = VirtualListState::new(1);
        first.sync(8, &mut state, &mut LayoutCx::new());
        let b_start = first.item_offset(&state, &"b").unwrap();
        state.scroll.set_vertical_offset(b_start.saturating_add(1));
        state.capture_anchor();

        let changed = VirtualList::new("messages")
            .item("new", 0, TextContent::new("inserted"))
            .item("a", 0, TextContent::new("short"))
            .item("b", 0, TextContent::new("b message wraps here"))
            .item("c", 0, TextContent::new("c"));
        changed.sync(6, &mut state, &mut LayoutCx::new());
        state.restore_anchor(3);

        assert_eq!(
            state.scroll.vertical_offset(),
            changed.item_offset(&state, &"b").unwrap().saturating_add(1)
        );
    }

    #[test]
    fn bottom_follow_and_ensure_visible_use_keyed_collection_geometry() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b"))
            .item("c", 0, TextContent::new("c"));
        let mut state = VirtualListState::new(1);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_follow_bottom(true);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(list.ensure_item_visible(&mut state, &"a", 2));
        assert_eq!(state.scroll.vertical_offset(), 0);
    }

    #[test]
    fn sync_retains_measurements_across_reorder_and_exposes_key_offsets() {
        let first = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b"));
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        first.sync(8, &mut state, &mut cx);
        let measured = cx.measured_nodes();

        let reordered = VirtualList::new("messages")
            .item("b", 0, TextContent::new("b"))
            .item("a", 0, TextContent::new("a"));
        reordered.sync(8, &mut state, &mut cx);

        assert_eq!(cx.measured_nodes(), measured);
        assert_eq!(reordered.item_offset(&state, &"a"), Some(2));
    }
}
