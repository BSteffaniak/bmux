//! Visible-only rendering for keyed variable-height component collections.

use std::collections::BTreeSet;

use bmux_tui::component::{
    Component, Constraints, Element, EventCx, LayoutCache, LayoutCx, LayoutEnvironment, LayoutId,
    LogicalRect,
};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::measured_list::MeasuredListIndex;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::semantic::SemanticRegion;

use crate::scroll_view::{ScrollViewState, scrollbar_state};

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
        let height = self.index.item(index).map_or(0, |item| item.height);
        let row = (*row).min(height.saturating_sub(1));
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll
            .set_vertical_offset(start.saturating_add(row).min(maximum));
        self.capture_anchor();
    }

    /// Scroll so one stable item begins at the viewport top.
    pub fn scroll_to_key(&mut self, key: &K, viewport_height: usize) -> bool {
        let Some(index) = self.index.index_of(key) else {
            return false;
        };
        let start = self.index.item_offset(index).unwrap_or(0);
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll.set_vertical_offset(start.min(maximum));
        true
    }

    /// Ensure one complete stable item is visible with minimum movement.
    pub fn ensure_key_visible(&mut self, key: &K, viewport_height: usize) -> bool {
        let Some(index) = self.index.index_of(key) else {
            return false;
        };
        let Some(item) = self.index.item(index) else {
            return false;
        };
        let start = self.index.item_offset(index).unwrap_or(0);
        let old = self.scroll.vertical_offset();
        let offset = crate::scroll_view::reveal_offset(old, viewport_height, start, item.height);
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll.set_vertical_offset(offset.min(maximum));
        self.scroll.vertical_offset() != old
    }

    /// Exact logical content extent at the synchronized width.
    #[must_use]
    pub fn total_height(&self) -> usize {
        self.index.total_height()
    }

    /// Convert exact virtual-list geometry into terminal scrollbar state.
    #[must_use]
    pub fn scrollbar_state(&self, viewport_height: usize) -> crate::scrollbar::ScrollbarState {
        scrollbar_state(
            self.index.total_height(),
            viewport_height,
            self.scroll.vertical_offset(),
        )
    }

    /// Scroll to the first logical collection row and stop following appends.
    pub const fn scroll_to_top(&mut self) {
        self.scroll.set_vertical_offset(0);
    }

    /// Move by a signed logical row delta and clamp to the collection extent.
    pub fn scroll_by(&mut self, rows: isize, viewport_height: usize) -> bool {
        let old = self.scroll.vertical_offset();
        let next = old.saturating_add_signed(rows);
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll.set_vertical_offset(next.min(maximum));
        self.scroll.vertical_offset() != old
    }

    /// Scroll to the final logical collection row and follow subsequent appends.
    pub fn scroll_to_bottom(&mut self, viewport_height: usize) {
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll.set_vertical_offset(maximum);
        self.scroll.set_follow_bottom(true);
    }

    /// Clamp logical scrolling to the current collection extent without changing
    /// whether subsequent appends are followed.
    pub fn clamp_scroll(&mut self, viewport_height: usize) {
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        let follow_bottom = self.scroll.follows_bottom();
        self.scroll
            .set_vertical_offset(self.scroll.vertical_offset().min(maximum));
        self.scroll.set_follow_bottom(follow_bottom);
    }

    /// Logical start row for a stable key.
    #[must_use]
    pub fn item_offset(&self, key: &K) -> Option<usize> {
        self.index
            .index_of(key)
            .and_then(|index| self.index.item_offset(index))
    }

    /// Stable key containing one logical collection row.
    #[must_use]
    pub fn key_at_offset(&self, offset: usize) -> Option<&K> {
        self.index
            .item_at_offset(offset)
            .and_then(|index| self.index.item(index))
            .map(|item| &item.key)
    }

    /// Retained layout cache diagnostics.
    #[must_use]
    pub const fn layout_cache(&self) -> &LayoutCache {
        &self.layouts
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

    /// Append one keyed item using the component's declared layout revision.
    #[must_use]
    pub fn component(mut self, key: K, component: impl Component + 'a) -> Self {
        let revision = component.revision();
        self.items
            .push(VirtualListItem::new(key, revision.layout, component));
        self
    }

    /// Append one keyed item with an externally managed layout revision.
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
    /// Panics if stable item keys or their string representations are duplicated.
    pub fn sync(&self, width: u16, state: &mut VirtualListState<K>, cx: &mut LayoutCx) {
        self.sync_with_environment(width, LayoutEnvironment::default(), state, cx);
    }

    /// Synchronize exact measurements with geometry-affecting terminal
    /// capability inputs included in both retained cache layers.
    ///
    /// # Panics
    ///
    /// Panics if stable item keys or their string representations are duplicated.
    pub fn sync_with_environment(
        &self,
        width: u16,
        environment: LayoutEnvironment,
        state: &mut VirtualListState<K>,
        cx: &mut LayoutCx,
    ) {
        let active = self
            .items
            .iter()
            .map(|item| item_layout_id(&self.id, &item.key))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            active.len(),
            self.items.len(),
            "virtual list keys must have unique string representations"
        );
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
            environment.capability_revision,
            |key| {
                let item = by_key.get(key).expect("synchronized key must exist");
                state
                    .layouts
                    .layout_with_revision_and_environment(
                        item_layout_id(&self.id, key),
                        item.component.as_component(),
                        item.layout_revision,
                        Constraints::for_width(width),
                        environment,
                        cx,
                    )
                    .size
                    .height
            },
        );
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
        let clip = cx.area();
        let first = usize::try_from(clip.y.max(0))
            .unwrap_or(usize::MAX)
            .min(usize::from(area.height));
        let end = usize::try_from(clip.y.saturating_add(i64::from(clip.height)).max(0))
            .unwrap_or(usize::MAX)
            .min(usize::from(area.height));
        let mut report = VirtualListRenderStats::default();
        if end <= first
            || clip.width == 0
            || clip.x >= i32::from(area.width)
            || clip.x.saturating_add(i32::from(clip.width)) <= 0
        {
            return report;
        }
        let range = state
            .index
            .visible_range(offset.saturating_add(first), end - first);
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
            let visible = translated_item_area(
                Rect::new(0, 0, area.width, area.height),
                local_y,
                measured.height,
            );
            cx.with_child(
                0,
                local_y,
                LocalRect::new(0, -local_y, area.width, area.height),
                |cx| cx.with_child_size(0, 0, layout.size, |cx| item.component.paint(layout, cx)),
            );
            let semantic_id = format!("{}.item.{}", self.id, item.key.to_string());
            cx.push_hit(HitRegion::new(semantic_id.clone(), visible).role(HitRole::ListItem));
            let visible_local = LocalRect::new(
                i32::from(visible.x),
                i64::from(visible.y),
                visible.width,
                visible.height,
            );
            cx.push_focus(semantic_id.clone(), visible_local);
            cx.push_semantic(SemanticRegion::new(semantic_id, visible, "list-item"));
            cx.push_damage(visible_local);
            report.painted_items = report.painted_items.saturating_add(1);
            report.registered_items = report.registered_items.saturating_add(1);
        }
        report
    }

    /// Route an event only through viewport-intersecting items, using the same
    /// retained layout, translation, clipping, and topmost-first order as paint.
    ///
    /// Mouse events are offered only to the visible item under the pointer.
    /// Non-positional events traverse visible items from bottom to top until
    /// handled.
    ///
    /// # Panics
    ///
    /// Panics if called before synchronizing the list at `area.width`.
    pub fn event(
        &self,
        area: Rect,
        state: &VirtualListState<K>,
        event: &Event,
        cx: &mut EventCx<'_>,
    ) -> EventOutcome {
        let offset = state.scroll.vertical_offset();
        let range = state.index.visible_range(offset, usize::from(area.height));
        let viewport = cx.visible_rect(LogicalRect::new(
            area.x,
            usize::from(area.y),
            area.width,
            usize::from(area.height),
        ));
        if viewport.is_empty() {
            return EventOutcome::Ignored;
        }
        let pointer = match event {
            Event::Mouse(mouse) if !viewport.contains(mouse.position) => {
                return EventOutcome::Ignored;
            }
            Event::Mouse(mouse) => Some(mouse.position),
            _ => None,
        };
        for index in (range.start..range.end).rev() {
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
            let local_area = translated_item_area(area, local_y, measured.height);
            let item_area = cx.visible_rect(LogicalRect::new(
                local_area.x,
                usize::from(local_area.y),
                local_area.width,
                usize::from(local_area.height),
            ));
            if item_area.is_empty() || pointer.is_some_and(|point| !item_area.contains(point)) {
                continue;
            }
            let outcome = cx.with_transform(
                0,
                start,
                i32::from(area.x),
                i64::from(area.y).saturating_add(local_y),
                item_area,
                |cx| cx.with_root(layout, |cx| item.component.event(event, layout, cx)),
            );
            if outcome.is_handled() {
                return outcome;
            }
            if pointer.is_some() {
                return EventOutcome::Ignored;
            }
        }
        EventOutcome::Ignored
    }

    /// Scroll so the keyed item is visible with the minimum movement.
    ///
    /// Returns whether the key exists, even if its viewport offset is unchanged.
    pub fn ensure_item_visible(
        &self,
        state: &mut VirtualListState<K>,
        key: &K,
        viewport_height: usize,
    ) -> bool {
        if state.index.index_of(key).is_none() {
            return false;
        }
        state.ensure_key_visible(key, viewport_height);
        true
    }

    /// Logical start row for a stable key.
    #[must_use]
    pub fn item_offset(&self, state: &VirtualListState<K>, key: &K) -> Option<usize> {
        state.item_offset(key)
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

fn translated_item_area(area: Rect, local_y: i64, item_height: usize) -> Rect {
    let top = i64::from(area.y).saturating_add(local_y);
    let bottom = top.saturating_add(i64::try_from(item_height).unwrap_or(i64::MAX));
    let visible_top = top.clamp(i64::from(area.y), i64::from(area.bottom()));
    let visible_bottom = bottom.clamp(visible_top, i64::from(area.bottom()));
    Rect::new(
        area.x,
        u16::try_from(visible_top).unwrap_or(area.y),
        area.width,
        u16::try_from(visible_bottom.saturating_sub(visible_top)).unwrap_or(u16::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::{VirtualList, VirtualListState};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{
        Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutEnvironment, LayoutId,
        LayoutNode, LogicalSize,
    };
    use bmux_tui::composition::TextBlock;
    use bmux_tui::damage::{Damage, DamagePolicy};
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitId;
    use bmux_tui::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement,
    };
    use bmux_tui::interaction::InteractionRouter;
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::selection::{SelectionFragment, SelectionScope};
    use bmux_tui::style::Style;
    use bmux_tui::text::{Line, Text};

    #[test]
    fn rejects_colliding_key_strings_before_changing_retained_state() {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
        struct Key(u8);

        impl std::fmt::Display for Key {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("same")
            }
        }

        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        VirtualList::new("keys")
            .item(Key(0), 0, TextBlock::new("original"))
            .sync(8, &mut state, &mut cx);
        let measurements = cx.measured_nodes();
        let height = state.total_height();
        for second in [0, 1] {
            let invalid = VirtualList::new("keys")
                .item(Key(0), 1, TextBlock::new("changed text wraps"))
                .item(Key(second), 0, TextBlock::new("collision"));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                invalid.sync(8, &mut state, &mut cx);
            }));
            assert!(result.is_err());
            assert_eq!(state.total_height(), height);
            assert_eq!(cx.measured_nodes(), measurements);
            assert_eq!(state.item_offset(&Key(0)), Some(0));
            assert_eq!(state.item_offset(&Key(1)), None);
        }
    }

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

    struct RevisedHeightItem {
        revision: u64,
        height: usize,
    }

    impl Component for RevisedHeightItem {
        fn revision(&self) -> ComponentRevision {
            ComponentRevision::new(self.revision, 0)
        }

        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new("revised-height"),
                constraints.constrain(LogicalSize::new(constraints.max_width(), self.height)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}
    }

    struct MetadataItem {
        id: &'static str,
        height: usize,
        cursor_row: Option<u16>,
    }

    impl Component for MetadataItem {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new(self.id),
                constraints.constrain(LogicalSize::new(constraints.max_width(), self.height)),
            )
        }

        fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
            let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
            cx.fill(
                LocalRect::new(0, 0, layout.size.width, height),
                self.id,
                Style::new(),
            );
            cx.push_selection_scope(SelectionScope::new(
                format!("scope:{}", self.id),
                Rect::new(0, 0, layout.size.width, height),
            ));
            cx.push_selection_fragment(SelectionFragment::new(
                format!("scope:{}", self.id),
                format!("content:{}", self.id),
                Rect::new(0, 1, 1, 1),
                0,
                0..1,
            ));
            cx.push_image(ImageContribution::Present(ImagePlacement {
                key: ImageKey::new(format!("image:{}", self.id)),
                payload: ImagePayload::Png {
                    bytes: vec![1],
                    width: 1,
                    height: 1,
                },
                destination: Rect::new(1, 0, 2, height),
                clip: Rect::new(0, 0, layout.size.width, height),
                lifecycle: ImageLifecycle::Frame,
            }));
            if let Some(row) = self.cursor_row {
                cx.set_cursor(Point::new(2, row), true);
            }
            cx.push_damage(LocalRect::new(0, 0, layout.size.width, height));
        }

        fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
            if matches!(event, Event::Mouse(_))
                && cx
                    .find_visible_rect(&layout.id)
                    .is_some_and(|area| !area.is_empty())
            {
                EventOutcome::Handled
            } else {
                EventOutcome::Ignored
            }
        }
    }

    #[test]
    fn tall_item_metadata_clips_before_terminal_conversion() {
        let list = VirtualList::new("tall").item(
            "item",
            0,
            MetadataItem {
                id: "tall-child",
                height: 100_000,
                cursor_row: None,
            },
        );
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(90_000);
        let viewport = Rect::new(4, 5, 8, 4);
        let area = Rect::new(0, 0, 8, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 12));
        let mut frame = Frame::new(&mut buffer);
        let mut report = super::VirtualListRenderStats::default();
        PaintCx::new(&mut frame).with_child(
            i32::from(viewport.x),
            i64::from(viewport.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| report = list.paint(area, &state, cx),
        );
        assert_eq!(report.registered_items, 1);
        assert_eq!(frame.hits().regions().len(), 2);
        for region in frame.hits().regions() {
            assert_eq!(region.area, viewport);
        }
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].area, viewport);
        assert_eq!(frame.semantics().regions()[0].id, "tall.item.item");
        let root = LayoutNode::leaf(LayoutId::new("root"), LogicalSize::new(20, 12));
        let mut event_cx = EventCx::with_clip(&root, viewport);
        for (point, expected) in [
            (Point::new(4, 5), EventOutcome::Handled),
            (Point::new(11, 8), EventOutcome::Handled),
            (Point::new(3, 5), EventOutcome::Ignored),
            (Point::new(4, 9), EventOutcome::Ignored),
        ] {
            let event = Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                point,
            ));
            assert_eq!(
                list.event(viewport, &state, &event, &mut event_cx),
                expected
            );
        }
        assert_eq!(super::translated_item_area(area, -90_000, 100_000), area);
        assert_eq!(
            super::translated_item_area(area, -100_000, 100_000).height,
            0
        );
        assert_eq!(super::translated_item_area(area, 10, 100_000).height, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn virtualized_metadata_uses_exact_boundary_clipping() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 3,
                    cursor_row: Some(0),
                },
            )
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 3,
                    cursor_row: Some(1),
                },
            )
            .item(
                "c",
                0,
                MetadataItem {
                    id: "c",
                    height: 3,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(6, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 4));
        let mut frame = Frame::new(&mut buffer);
        let report = list.paint(Rect::new(0, 0, 6, 4), &state, &mut PaintCx::new(&mut frame));

        assert_eq!(report.painted_items, 2);
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("a")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 1))
                .map(|cell| cell.symbol.as_str()),
            Some("b")
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(0, 3))
                .map(|cell| cell.symbol.as_str()),
            Some("b")
        );
        assert_eq!(report.registered_items, 2);
        let pointer_hits = frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.pointer_events)
            .collect::<Vec<_>>();
        assert_eq!(pointer_hits.len(), 2);
        assert_eq!(pointer_hits[0].area, Rect::new(0, 0, 6, 1));
        assert_eq!(pointer_hits[1].area, Rect::new(0, 1, 6, 3));
        assert_eq!(frame.semantics().regions().len(), 2);
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 6, 1));
        assert_eq!(frame.semantics().regions()[1].area, Rect::new(0, 1, 6, 3));
        assert_eq!(frame.selection().scopes().len(), 2);
        assert_eq!(frame.selection().scopes()[0].area, Rect::new(0, 0, 6, 1));
        assert_eq!(frame.selection().scopes()[1].area, Rect::new(0, 1, 6, 3));
        assert_eq!(frame.selection().fragments().len(), 1);
        assert_eq!(frame.selection().fragments()[0].area, Rect::new(0, 2, 1, 1));
        assert_eq!(frame.images().len(), 2);
        let ImageContribution::Present(first_image) = &frame.images()[0] else {
            panic!("expected image placement");
        };
        assert_eq!(first_image.key, ImageKey::new("image:a"));
        assert_eq!(first_image.destination, Rect::new(1, 0, 2, 1));
        assert_eq!(first_image.clip, Rect::new(0, 0, 6, 1));
        assert_eq!(first_image.lifecycle, ImageLifecycle::Frame);
        let ImageContribution::Present(second_image) = &frame.images()[1] else {
            panic!("expected image placement");
        };
        assert_eq!(second_image.key, ImageKey::new("image:b"));
        assert_eq!(second_image.destination, Rect::new(1, 1, 2, 3));
        assert_eq!(second_image.clip, Rect::new(0, 1, 6, 3));
        assert_eq!(second_image.lifecycle, ImageLifecycle::Frame);
        assert_eq!(
            frame.cursor(),
            Some(bmux_tui::frame::Cursor::visible(Point::new(2, 2)))
        );
        assert_eq!(
            frame.damage(bmux_tui::damage::DamagePolicy {
                max_regions: 64,
                max_area_percent: 101,
            }),
            bmux_tui::damage::Damage::Regions(vec![Rect::new(0, 0, 6, 4)])
        );

        let root = LayoutNode::leaf(LayoutId::new("root"), LogicalSize::new(6, 4));
        let mut event_cx = EventCx::with_clip(&root, Rect::new(0, 0, 6, 4));
        let clipped_item = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(2, 0),
        ));
        assert_eq!(
            list.event(Rect::new(0, 0, 6, 4), &state, &clipped_item, &mut event_cx),
            EventOutcome::Handled
        );
        let visible_item = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(2, 2),
        ));
        assert_eq!(
            list.event(Rect::new(0, 0, 6, 4), &state, &visible_item, &mut event_cx),
            EventOutcome::Handled
        );
    }

    struct EventItem {
        id: &'static str,
        height: usize,
        outcome: EventOutcome,
    }

    impl Component for EventItem {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new(self.id),
                constraints.constrain(LogicalSize::new(constraints.max_width(), self.height)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
            if matches!(event, Event::Mouse(_))
                && cx
                    .find_visible_rect(&layout.id)
                    .is_some_and(|area| !area.is_empty())
            {
                self.outcome
            } else {
                EventOutcome::Ignored
            }
        }
    }

    #[test]
    fn nonpositional_events_skip_parent_clipped_items() {
        struct Handler(EventOutcome);
        impl Component for Handler {
            fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf(
                    "handler".into(),
                    constraints.constrain(LogicalSize::new(8, 2)),
                )
            }
            fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}
            fn event(
                &self,
                _event: &Event,
                _layout: &LayoutNode,
                _cx: &mut EventCx<'_>,
            ) -> EventOutcome {
                self.0
            }
        }
        let list = VirtualList::new("list")
            .item("visible", 0, Handler(EventOutcome::Handled))
            .item("hidden", 0, Handler(EventOutcome::Redraw));
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        let root = LayoutNode::leaf("root".into(), LogicalSize::new(8, 4));
        let area = Rect::new(0, 0, 8, 4);
        let event = Event::Paste("input".into());
        let mut cx = EventCx::with_clip(&root, Rect::new(0, 0, 8, 2));
        assert_eq!(
            list.event(area, &state, &event, &mut cx),
            EventOutcome::Handled
        );
        let mut cx = EventCx::with_clip(&root, Rect::new(10, 10, 2, 2));
        assert_eq!(
            list.event(area, &state, &event, &mut cx),
            EventOutcome::Ignored
        );
        let mut cx = EventCx::with_clip(&root, area);
        assert_eq!(
            list.event(area, &state, &event, &mut cx),
            EventOutcome::Redraw
        );
    }

    #[test]
    fn intrinsic_narrow_item_uses_full_width_list_constraints() {
        struct NarrowItem;
        impl Component for NarrowItem {
            fn layout(&self, constraints: Constraints, _: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf(
                    "narrow".into(),
                    constraints.constrain(LogicalSize::new(3, 2)),
                )
            }

            fn paint(&self, _: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
                cx.fill(cx.area(), "x", Style::new());
            }

            fn event(&self, _: &Event, _: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
                assert_eq!(cx.clip(), Some(Rect::new(12, 11, 8, 2)));
                EventOutcome::Handled
            }
        }

        let list = VirtualList::new("list").item("narrow", 0, NarrowItem);
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        let root = LayoutNode::leaf("root".into(), LogicalSize::new(30, 30));
        let area = Rect::new(2, 1, 8, 4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 30));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(12, 11, LocalRect::new(0, 0, 8, 4), |cx| {
            list.paint(Rect::new(0, 0, 8, 4), &state, cx);
        });
        for y in 10..14 {
            for x in 11..21 {
                let expected = if (12..20).contains(&x) && (11..13).contains(&y) {
                    "x"
                } else {
                    " "
                };
                assert_eq!(buffer.get(Point::new(x, y)).unwrap().symbol, expected);
            }
        }
        let mut cx = EventCx::new(&root);
        cx.with_transform(0, 0, 10, 10, Rect::new(0, 0, 30, 30), |cx| {
            for (x, expected) in [
                (12, EventOutcome::Handled),
                (14, EventOutcome::Handled),
                (15, EventOutcome::Handled),
                (19, EventOutcome::Handled),
                (20, EventOutcome::Ignored),
            ] {
                let event = Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(x, 11),
                ));
                assert_eq!(list.event(area, &state, &event, cx), expected);
            }
            assert_eq!(
                list.event(area, &state, &Event::Paste("input".into()), cx),
                EventOutcome::Handled
            );
        });
        // A parent exposing only rows below the measured child hides it entirely,
        // including from non-positional handlers that unconditionally accept input.
        let mut cx = EventCx::with_clip(&root, Rect::new(2, 3, 8, 2));
        assert_eq!(
            list.event(area, &state, &Event::Paste("input".into()), &mut cx),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn translated_list_routes_pointer_using_parent_clip() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                EventItem {
                    id: "a",
                    height: 3,
                    outcome: EventOutcome::Handled,
                },
            )
            .item(
                "b",
                0,
                EventItem {
                    id: "b",
                    height: 3,
                    outcome: EventOutcome::Redraw,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let root = LayoutNode::leaf("root".into(), LogicalSize::new(30, 30));
        let mut cx = EventCx::with_clip(&root, Rect::new(0, 0, 30, 30));
        // Local area (2, 1) is presented at (12, 11), with its final row clipped.
        let area = Rect::new(2, 1, 8, 4);
        cx.with_transform(0, 0, 10, 10, Rect::new(12, 11, 8, 3), |cx| {
            for (x, y, expected) in [
                (13, 11, EventOutcome::Handled),
                (13, 12, EventOutcome::Redraw),
                (13, 13, EventOutcome::Redraw),
                (13, 14, EventOutcome::Ignored),
                (11, 11, EventOutcome::Ignored),
                (3, 1, EventOutcome::Ignored),
            ] {
                let event = Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(x, y),
                ));
                assert_eq!(list.event(area, &state, &event, cx), expected);
            }
        });
    }

    #[test]
    fn pointer_events_route_only_to_visible_intersecting_items() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                EventItem {
                    id: "a",
                    height: 3,
                    outcome: EventOutcome::Handled,
                },
            )
            .item(
                "b",
                0,
                EventItem {
                    id: "b",
                    height: 3,
                    outcome: EventOutcome::Redraw,
                },
            )
            .item(
                "c",
                0,
                EventItem {
                    id: "c",
                    height: 3,
                    outcome: EventOutcome::Handled,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let root = LayoutNode::leaf(LayoutId::new("root"), LogicalSize::new(8, 4));
        let area = Rect::new(4, 5, 8, 4);
        let mut cx = EventCx::with_clip(&root, area);

        let first = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(5, 5),
        ));
        assert_eq!(
            list.event(area, &state, &first, &mut cx),
            EventOutcome::Handled
        );

        let lower_pointer = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(5, 8),
        ));
        assert_eq!(
            list.event(area, &state, &lower_pointer, &mut cx),
            EventOutcome::Redraw
        );

        let outside = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(3, 5),
        ));
        assert_eq!(
            list.event(area, &state, &outside, &mut cx),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn semantic_ids_remain_keyed_across_reorder() {
        let first = VirtualList::new("messages")
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 1,
                    cursor_row: None,
                },
            )
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 1,
                    cursor_row: None,
                },
            );
        let reordered = VirtualList::new("messages")
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 1,
                    cursor_row: None,
                },
            )
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 1,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(0);
        let area = Rect::new(0, 0, 8, 2);

        first.sync(8, &mut state, &mut LayoutCx::new());
        let first_ids = paint_semantic_ids(&first, &state, area);
        reordered.sync(8, &mut state, &mut LayoutCx::new());
        let reordered_ids = paint_semantic_ids(&reordered, &state, area);

        assert_eq!(first_ids, ["messages.item.a", "messages.item.b"]);
        assert_eq!(reordered_ids, ["messages.item.b", "messages.item.a"]);
    }

    fn paint_semantic_ids<K>(
        list: &VirtualList<'_, K>,
        state: &VirtualListState<K>,
        area: Rect,
    ) -> Vec<String>
    where
        K: Clone + Ord + ToString,
    {
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        list.paint(area, state, &mut PaintCx::new(&mut frame));
        frame
            .semantics()
            .regions()
            .iter()
            .map(|region| region.id.as_str().to_owned())
            .collect()
    }

    #[test]
    fn stable_key_scroll_and_ensure_visible_use_exact_item_geometry() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b line that wraps"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        list.sync(6, &mut state, &mut LayoutCx::new());

        assert!(state.scroll_to_key(&"b", 3));
        assert_eq!(state.scroll.vertical_offset(), 2);
        assert!(!state.ensure_key_visible(&"b", 3));
        assert!(state.ensure_key_visible(&"c", 3));
        assert_eq!(state.scroll.vertical_offset(), 4);
        assert!(!state.scroll_to_key(&"missing", 3));
        assert!(!state.ensure_key_visible(&"missing", 3));
        assert_eq!(state.scroll.vertical_offset(), 4);
    }

    #[test]
    fn focused_key_is_ensured_visible_and_restored_after_modal_scope() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());

        let focused_key = "d";
        assert!(state.ensure_key_visible(&focused_key, 2));
        assert_eq!(state.scroll.vertical_offset(), 2);

        let area = Rect::new(0, 0, 8, 2);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        list.paint(area, &state, &mut PaintCx::new(&mut frame));
        let focused_id = HitId::new("messages.item.d");
        let mut router = InteractionRouter::new();
        router.commit_scene(frame.hits().clone(), None);
        assert!(router.set_focused(&focused_id));

        let modal = bmux_tui::hit::HitRegion::new("modal.close", area)
            .focusable(true)
            .focus_scope("modal");
        let modal_scene = frame.hits().clone().with_region(modal);
        router.commit_scene(modal_scene, Some(HitId::new("modal")));
        assert_eq!(router.focused(), Some(&HitId::new("modal.close")));
        router.commit_scene(frame.hits().clone(), None);
        assert_eq!(router.focused(), Some(&focused_id));
    }

    #[test]
    fn capability_revision_invalidates_item_measurements_and_layouts() {
        let list =
            VirtualList::new("messages").item("a", 0, TextBlock::new("a message that wraps"));
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();

        for revision in [1, 1, 2] {
            list.sync_with_environment(8, LayoutEnvironment::new(revision), &mut state, &mut cx);
        }

        assert_eq!(cx.measured_nodes(), 2);
        assert_eq!(state.key_at_offset(0), Some(&"a"));
        assert_eq!(state.item_offset(&"a"), Some(0));
    }

    #[test]
    fn logical_navigation_clamps_and_disables_bottom_follow() {
        fn rows(lines: &[&str]) -> TextBlock {
            TextBlock::new(Text::from_lines(
                lines
                    .iter()
                    .map(|line| Line::raw(*line))
                    .collect::<Vec<_>>(),
            ))
        }

        let mut state = VirtualListState::new(0);
        VirtualList::new("messages")
            .item("a", 0, rows(&["one", "two", "three", "four", "five"]))
            .sync(8, &mut state, &mut LayoutCx::new());

        state.scroll_to_bottom(2);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(state.scroll.follows_bottom());
        assert!(state.scroll_by(-2, 2));
        assert_eq!(state.scroll.vertical_offset(), 1);
        assert!(!state.scroll.follows_bottom());
        assert!(state.scroll_by(isize::MAX, 2));
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(!state.scroll_by(1, 2));
        state.scroll_to_top();
        assert_eq!(state.scroll.vertical_offset(), 0);
        assert!(!state.scroll.follows_bottom());
    }

    #[test]
    fn scroll_to_bottom_sets_exact_offset_and_follows_appends() {
        fn rows(lines: &[&str]) -> TextBlock {
            TextBlock::new(Text::from_lines(
                lines
                    .iter()
                    .map(|line| Line::raw(*line))
                    .collect::<Vec<_>>(),
            ))
        }

        let mut state = VirtualListState::new(0);
        VirtualList::new("messages")
            .item("a", 0, rows(&["one", "two", "three"]))
            .sync(8, &mut state, &mut LayoutCx::new());

        state.scroll_to_bottom(2);
        assert_eq!(state.scroll.vertical_offset(), 1);
        assert!(state.scroll.follows_bottom());

        state.capture_anchor();
        VirtualList::new("messages")
            .item("a", 0, rows(&["one", "two", "three"]))
            .item("b", 0, rows(&["four", "five"]))
            .sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert!(state.scroll.follows_bottom());
    }

    #[test]
    fn state_geometry_queries_hide_index_implementation() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps here"));
        let mut state = VirtualListState::new(1);
        list.sync(6, &mut state, &mut LayoutCx::new());

        assert_eq!(state.total_height(), 5);
        assert_eq!(state.item_offset(&"a"), Some(0));
        assert_eq!(state.item_offset(&"b"), Some(2));
        assert_eq!(state.key_at_offset(0), Some(&"a"));
        assert_eq!(state.key_at_offset(1), Some(&"a"));
        assert_eq!(state.key_at_offset(2), Some(&"b"));
        assert_eq!(state.key_at_offset(4), Some(&"b"));
        assert_eq!(state.key_at_offset(5), None);
    }

    #[test]
    fn viewport_height_does_not_remeasure_but_width_and_removed_keys_do() {
        let list = VirtualList::new("messages")
            .item("a", 1, TextBlock::new("a message that wraps"))
            .item("b", 1, TextBlock::new("b"));
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
            .item("b", 1, TextBlock::new("b"))
            .item("a", 1, TextBlock::new("a message that wraps"));
        reordered.sync(8, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), initial_measurements);

        reordered.sync(6, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), initial_measurements + 2);
        let retained_before_removal = state.layout_cache().len();

        VirtualList::new("messages")
            .item("a", 1, TextBlock::new("a message that wraps"))
            .sync(6, &mut state, &mut cx);
        assert!(state.layout_cache().len() < retained_before_removal);
        assert!(state.layout_cache().stats().released > 0);
    }

    #[test]
    fn component_item_repaints_without_remeasuring_for_paint_only_revision() {
        let area = Rect::new(0, 0, 8, 1);
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        let initial = VirtualList::new("messages").component(
            "a",
            TextBlock::new("message").style(Style::new().fg(bmux_tui::style::Color::Red)),
        );
        initial.sync(area.width, &mut state, &mut cx);
        let measured = cx.measured_nodes();

        let changed = VirtualList::new("messages").component(
            "a",
            TextBlock::new("message").style(Style::new().fg(bmux_tui::style::Color::Blue)),
        );
        changed.sync(area.width, &mut state, &mut cx);
        assert_eq!(cx.measured_nodes(), measured);

        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        changed.paint(area, &state, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().get(Point::new(0, 0)).unwrap().style.fg,
            Some(bmux_tui::style::Color::Blue)
        );
    }

    #[test]
    fn component_item_uses_declared_layout_revision() {
        let mut state = VirtualListState::new(0);
        let mut cx = LayoutCx::new();
        VirtualList::new("messages")
            .component(
                "a",
                RevisedHeightItem {
                    revision: 1,
                    height: 1,
                },
            )
            .sync(8, &mut state, &mut cx);
        assert_eq!(state.total_height(), 1);

        VirtualList::new("messages")
            .component(
                "a",
                RevisedHeightItem {
                    revision: 2,
                    height: 3,
                },
            )
            .sync(8, &mut state, &mut cx);
        assert_eq!(state.total_height(), 3);
        assert_eq!(cx.measured_nodes(), 2);
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
    fn item_paint_scope_cannot_fill_gaps_or_neighbors() {
        struct FillItem;
        impl Component for FillItem {
            fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
                LayoutNode::leaf("fill".into(), constraints.constrain(LogicalSize::new(4, 1)))
            }
            fn paint(&self, _layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
                cx.fill(cx.area(), "x", bmux_tui::style::Style::default());
            }
        }
        let list = VirtualList::new("list")
            .item("first", 0, TextBlock::new("a"))
            .item("fill", 0, FillItem)
            .item("last", 0, TextBlock::new("b"));
        let mut state = VirtualListState::new(1);
        list.sync(4, &mut state, &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 5));
        let mut frame = Frame::new(&mut buffer);
        list.paint(Rect::new(0, 0, 4, 5), &state, &mut PaintCx::new(&mut frame));
        for (row, expected) in ["a   ", "    ", "xxxx", "    ", "b   "].iter().enumerate() {
            assert_eq!(
                frame
                    .buffer()
                    .row_symbols(u16::try_from(row).unwrap())
                    .as_deref(),
                Some(*expected)
            );
        }
    }

    #[test]
    fn parent_clip_limits_virtual_list_paint_work() {
        let list = VirtualList::new("list")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(0);
        list.sync(4, &mut state, &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(0, -2, LocalRect::new(0, 0, 4, 4), |cx| {
            let report = list.paint(Rect::new(0, 0, 4, 4), &state, cx);
            assert_eq!(report.painted_items, 1);
            assert_eq!(report.registered_items, 1);
        });
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("c   "));
        assert_eq!(frame.semantics().regions().len(), 1);
        PaintCx::new(&mut frame).with_child(10, 0, LocalRect::new(0, 0, 4, 4), |cx| {
            assert_eq!(
                list.paint(Rect::new(0, 0, 4, 4), &state, cx).painted_items,
                0
            );
        });
    }

    #[test]
    fn paints_and_registers_only_intersecting_variable_height_items() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b line that wraps"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(1);
        list.sync(6, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 3));
        let mut frame = Frame::new(&mut buffer);
        let report = list.paint(Rect::new(0, 0, 6, 3), &state, &mut PaintCx::new(&mut frame));

        assert_eq!(report.painted_items, 1);
        assert_eq!(frame.hits().regions().len(), 2);
        assert_eq!(frame.hits().regions()[0].id.as_str(), "messages.item.b");
        assert!(frame.hits().regions()[0].pointer_events);
        assert_eq!(frame.hits().regions()[1].id.as_str(), "messages.item.b");
        assert!(!frame.hits().regions()[1].pointer_events);
        assert!(frame.hits().regions()[1].focusable);
        assert_eq!(frame.hits().regions()[1].area, Rect::new(0, 0, 6, 3));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].id, "messages.item.b");
        assert_eq!(frame.semantics().regions()[0].role, "list-item");
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 6, 3));
        assert_eq!(
            frame.damage(bmux_tui::damage::DamagePolicy {
                max_regions: 64,
                max_area_percent: 101,
            }),
            bmux_tui::damage::Damage::Regions(vec![Rect::new(0, 0, 6, 3)])
        );
    }

    #[test]
    fn clips_partial_first_and_last_items_to_exact_viewport_rows() {
        let list = VirtualList::new("messages")
            .item(
                "a",
                0,
                MetadataItem {
                    id: "a",
                    height: 3,
                    cursor_row: None,
                },
            )
            .item(
                "b",
                0,
                MetadataItem {
                    id: "b",
                    height: 3,
                    cursor_row: None,
                },
            )
            .item(
                "c",
                0,
                MetadataItem {
                    id: "c",
                    height: 3,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(0);
        list.sync(6, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(1);
        let area = Rect::new(0, 0, 6, 4);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        let report = list.paint(area, &state, &mut PaintCx::new(&mut frame));
        let pointer_hits = frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.pointer_events)
            .collect::<Vec<_>>();

        assert_eq!(report.painted_items, 2);
        assert_eq!(pointer_hits.len(), 2);
        assert_eq!(pointer_hits[0].id.as_str(), "messages.item.a");
        assert_eq!(pointer_hits[0].area, Rect::new(0, 0, 6, 2));
        assert_eq!(pointer_hits[1].id.as_str(), "messages.item.b");
        assert_eq!(pointer_hits[1].area, Rect::new(0, 2, 6, 2));
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 6, 2));
        assert_eq!(frame.semantics().regions()[1].area, Rect::new(0, 2, 6, 2));
        assert!(
            frame
                .hits()
                .regions()
                .iter()
                .all(|region| { region.id.as_str() != "messages.item.c" })
        );
    }

    #[test]
    fn boundary_items_are_the_only_registered_interactions() {
        let list = (0..100usize).fold(VirtualList::new("messages"), |list, key| {
            list.item(
                key,
                0,
                MetadataItem {
                    id: "item",
                    height: 3,
                    cursor_row: None,
                },
            )
        });
        let mut state = VirtualListState::new(1);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(2);
        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        let report = list.paint(area, &state, &mut PaintCx::new(&mut frame));
        let pointer_ids = frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.pointer_events)
            .map(|region| region.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(report.painted_items, 2);
        assert_eq!(report.registered_items, 2);
        assert_eq!(pointer_ids, ["messages.item.0", "messages.item.1"]);
        assert_eq!(frame.semantics().regions().len(), 2);
        assert_eq!(frame.selection().fragments().len(), 1);
        assert_eq!(frame.images().len(), 2);
        assert!(matches!(
            frame.damage(DamagePolicy {
                max_regions: usize::MAX,
                max_area_percent: 101,
            }),
            Damage::Regions(ref regions)
                if !regions.is_empty()
                    && regions.len() <= 2
                    && regions.iter().all(|region| area.intersection(*region) == *region)
        ));
    }

    #[test]
    fn large_collection_paint_work_is_bounded_by_viewport() {
        let mut list = VirtualList::new("messages");
        for key in 0..10_000usize {
            list = list.item(key, 0, TextBlock::new("x"));
        }
        let mut state = VirtualListState::new(0);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_vertical_offset(7_500);

        let area = Rect::new(0, 0, 8, 5);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        let report = list.paint(area, &state, &mut PaintCx::new(&mut frame));

        assert_eq!(report.painted_items, 5);
        assert_eq!(report.registered_items, 5);
        assert_eq!(
            frame
                .hits()
                .regions()
                .iter()
                .filter(|region| region.pointer_events)
                .count(),
            5
        );
        assert_eq!(frame.semantics().regions().len(), 5);
    }

    #[test]
    fn removed_anchor_falls_forward_then_back_and_empty_resets() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state
            .scroll
            .set_vertical_offset(initial.item_offset(&state, &"b").unwrap());
        state.capture_anchor();

        let removed = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("c", 0, TextBlock::new("c"));
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
            .item("a", 0, TextBlock::new("short"))
            .item("b", 0, TextBlock::new("b message wraps here"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        first.sync(8, &mut state, &mut LayoutCx::new());
        let b_start = first.item_offset(&state, &"b").unwrap();
        state.scroll.set_vertical_offset(b_start.saturating_add(1));
        state.capture_anchor();

        let changed = VirtualList::new("messages")
            .item("new", 0, TextBlock::new("inserted"))
            .item("a", 0, TextBlock::new("short"))
            .item("b", 0, TextBlock::new("b message wraps here"))
            .item("c", 0, TextBlock::new("c"));
        changed.sync(6, &mut state, &mut LayoutCx::new());
        state.restore_anchor(3);

        assert_eq!(
            state.scroll.vertical_offset(),
            changed.item_offset(&state, &"b").unwrap().saturating_add(1)
        );
    }

    #[test]
    fn stable_viewport_survives_complete_keyed_mutation_sequence() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("d", 0, TextBlock::new("d"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state
            .scroll
            .set_vertical_offset(state.item_offset(&"c").unwrap().saturating_add(1));
        state.capture_anchor();

        let appended = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("d", 0, TextBlock::new("d"))
            .item("e", 0, TextBlock::new("e"));
        assert_anchor_after_sync(&appended, 8, &mut state, "c", 1);

        let inserted = VirtualList::new("messages")
            .item("new", 0, TextBlock::new("inserted above"))
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("d", 0, TextBlock::new("d"))
            .item("e", 0, TextBlock::new("e"));
        assert_anchor_after_sync(&inserted, 8, &mut state, "c", 1);

        let reordered = VirtualList::new("messages")
            .item("e", 0, TextBlock::new("e"))
            .item("d", 0, TextBlock::new("d"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("a", 0, TextBlock::new("a"))
            .item("new", 0, TextBlock::new("inserted above"));
        assert_anchor_after_sync(&reordered, 8, &mut state, "c", 1);
        assert_anchor_after_sync(&reordered, 6, &mut state, "c", 1);

        let removed_above = VirtualList::new("messages")
            .item("e", 0, TextBlock::new("e"))
            .item("c", 0, TextBlock::new("c wraps across rows"))
            .item("b", 0, TextBlock::new("b wraps across rows"))
            .item("a", 0, TextBlock::new("a"))
            .item("new", 0, TextBlock::new("inserted above"));
        assert_anchor_after_sync(&removed_above, 6, &mut state, "c", 1);
    }

    fn assert_anchor_after_sync<'a>(
        list: &VirtualList<'_, &'a str>,
        width: u16,
        state: &mut VirtualListState<&'a str>,
        key: &str,
        row: usize,
    ) {
        list.sync(width, state, &mut LayoutCx::new());
        state.restore_anchor(3);
        assert_eq!(
            state.scroll.vertical_offset(),
            state.item_offset(&key).unwrap().saturating_add(row)
        );
    }

    #[test]
    fn bottom_follow_and_ensure_visible_use_keyed_collection_geometry() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        list.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_follow_bottom(true);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);
        assert_eq!(state.total_height(), 5);
        let scrollbar = state.scrollbar_state(2);
        assert_eq!(scrollbar.content_len, 5);
        assert_eq!(scrollbar.viewport_len, 2);
        assert_eq!(scrollbar.offset, 3);
        assert_eq!(scrollbar.max_offset(), 3);
        assert!(list.ensure_item_visible(&mut state, &"a", 2));
        assert_eq!(state.scroll.vertical_offset(), 0);
    }

    #[test]
    fn list_and_state_visibility_share_offsets_and_follow_policy() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("first"))
            .item("b", 0, TextBlock::new("several wrapped words"))
            .item("c", 0, TextBlock::new("last"));
        for viewport in [0, 1, 3, 100] {
            for offset in [0, 2, usize::MAX] {
                for key in ["a", "b", "c", "missing"] {
                    let mut through_list = VirtualListState::new(1);
                    let mut through_state = VirtualListState::new(1);
                    for state in [&mut through_list, &mut through_state] {
                        list.sync(5, state, &mut LayoutCx::new());
                        state.scroll.set_vertical_offset(offset);
                        state.scroll.set_follow_bottom(true);
                    }
                    let found = list.ensure_item_visible(&mut through_list, &key, viewport);
                    let changed = through_state.ensure_key_visible(&key, viewport);
                    assert_eq!(found, key != "missing");
                    assert_eq!(changed, through_state.scroll.vertical_offset() != offset);
                    assert_eq!(
                        through_list.scroll.vertical_offset(),
                        through_state.scroll.vertical_offset()
                    );
                    assert_eq!(
                        through_list.scroll.follows_bottom(),
                        through_state.scroll.follows_bottom()
                    );
                    if !found {
                        assert_eq!(through_list.scroll.vertical_offset(), offset);
                        assert!(through_list.scroll.follows_bottom());
                    }
                }
            }
        }
    }

    #[test]
    fn shrinking_anchor_item_keeps_the_surviving_key_visible() {
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        let initial = VirtualList::new("reflow")
            .item("anchor", 0, TextBlock::new("a\nb\nc\nd\ne"))
            .item("following", 0, TextBlock::new("1\n2\n3\n4\n5\n6"));
        initial.sync(8, &mut state, &mut cx);
        state.scroll.set_vertical_offset(4);
        state.capture_anchor();
        let shorter = VirtualList::new("reflow")
            .item("anchor", 1, TextBlock::new("a\nb"))
            .item("following", 0, TextBlock::new("1\n2\n3\n4\n5\n6"));
        shorter.sync(8, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 1);
        assert_eq!(state.key_at_offset(1), Some(&"anchor"));
        assert!(!state.scroll.follows_bottom());
        // Subsequent reflow retains the clamped row, not the stale original row.
        initial.sync(8, &mut state, &mut cx);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 1);
    }

    #[test]
    fn clamping_preserves_follow_policy_across_append_and_viewport_changes() {
        let initial = VirtualList::new("list")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"));
        let appended = VirtualList::new("list")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        for follow in [false, true] {
            let mut state = VirtualListState::new(0);
            initial.sync(8, &mut state, &mut LayoutCx::new());
            state.scroll.set_vertical_offset(100);
            state.scroll.set_follow_bottom(follow);
            state.clamp_scroll(1);
            assert_eq!(state.scroll.vertical_offset(), 1);
            assert_eq!(state.scroll.follows_bottom(), follow);
            state.clamp_scroll(3);
            assert_eq!(state.scroll.vertical_offset(), 0);
            assert_eq!(state.scroll.follows_bottom(), follow);
            appended.sync(8, &mut state, &mut LayoutCx::new());
            state.restore_anchor(1);
            assert_eq!(state.scroll.vertical_offset(), if follow { 2 } else { 0 });
            assert_eq!(state.scroll.follows_bottom(), follow);
        }
    }

    #[test]
    fn bottom_follow_survives_append_without_jump() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_follow_bottom(true);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);

        let appended = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"))
            .item("c", 0, TextBlock::new("c"))
            .item("d", 0, TextBlock::new("d message wraps"));
        appended.sync(8, &mut state, &mut LayoutCx::new());
        state.restore_anchor(2);

        assert!(state.scroll.follows_bottom());
        assert_eq!(
            state.scroll.vertical_offset(),
            state.total_height().saturating_sub(2)
        );
    }

    #[test]
    fn generic_component_measurements_retain_by_key_not_rendered_rows() {
        let first = VirtualList::new("surfaces")
            .item(
                "panel",
                7,
                MetadataItem {
                    id: "panel",
                    height: 3,
                    cursor_row: None,
                },
            )
            .item(
                "toolbar",
                11,
                MetadataItem {
                    id: "toolbar",
                    height: 2,
                    cursor_row: None,
                },
            );
        let mut state = VirtualListState::new(2);
        let mut cx = LayoutCx::new();
        first.sync(20, &mut state, &mut cx);
        let measured = cx.measured_nodes();

        let reordered = VirtualList::new("surfaces")
            .item(
                "toolbar",
                11,
                MetadataItem {
                    id: "toolbar",
                    height: 2,
                    cursor_row: None,
                },
            )
            .item(
                "panel",
                7,
                MetadataItem {
                    id: "panel",
                    height: 3,
                    cursor_row: None,
                },
            );
        reordered.sync(20, &mut state, &mut cx);

        assert_eq!(cx.measured_nodes(), measured);
        assert_eq!(state.total_height(), 7);
        assert_eq!(state.item_offset(&"toolbar"), Some(0));
        assert_eq!(state.item_offset(&"panel"), Some(4));
        assert_eq!(state.key_at_offset(3), Some(&"toolbar"));
        assert_eq!(state.key_at_offset(4), Some(&"panel"));
    }

    #[test]
    fn sync_retains_measurements_across_reorder_and_exposes_key_offsets() {
        let first = VirtualList::new("messages")
            .item("a", 0, TextBlock::new("a"))
            .item("b", 0, TextBlock::new("b"));
        let mut state = VirtualListState::new(1);
        let mut cx = LayoutCx::new();
        first.sync(8, &mut state, &mut cx);
        let measured = cx.measured_nodes();

        let reordered = VirtualList::new("messages")
            .item("b", 0, TextBlock::new("b"))
            .item("a", 0, TextBlock::new("a"));
        reordered.sync(8, &mut state, &mut cx);

        assert_eq!(cx.measured_nodes(), measured);
        assert_eq!(reordered.item_offset(&state, &"a"), Some(2));
    }
}
