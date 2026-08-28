//! Visible-only rendering for keyed variable-height component collections.

use std::collections::BTreeSet;

use bmux_tui::component::{
    Component, Constraints, Element, EventCx, LayoutCache, LayoutCx, LayoutEnvironment, LayoutId,
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
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll
            .set_vertical_offset(start.saturating_add(*row).min(maximum));
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
        let end = start.saturating_add(item.height);
        let old = self.scroll.vertical_offset();
        let mut offset = old;
        if start < offset {
            offset = start;
        } else if end > offset.saturating_add(viewport_height) {
            offset = end.saturating_sub(viewport_height);
        }
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

    /// Clamp logical scrolling to the current collection extent.
    pub fn clamp_scroll(&mut self, viewport_height: usize) {
        let maximum = self.index.total_height().saturating_sub(viewport_height);
        self.scroll
            .set_vertical_offset(self.scroll.vertical_offset().min(maximum));
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
        self.sync_with_environment(width, LayoutEnvironment::default(), state, cx);
    }

    /// Synchronize exact measurements with geometry-affecting terminal
    /// capability inputs included in both retained cache layers.
    ///
    /// # Panics
    ///
    /// Panics if duplicate stable item keys are supplied.
    pub fn sync_with_environment(
        &self,
        width: u16,
        environment: LayoutEnvironment,
        state: &mut VirtualListState<K>,
        cx: &mut LayoutCx,
    ) {
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
                    let semantic_id = format!("{}.item.{}", self.id, item.key.to_string());
                    cx.push_hit(
                        HitRegion::new(
                            semantic_id.clone(),
                            Rect::new(0, 0, area.width, item_height),
                        )
                        .role(HitRole::ListItem),
                    );
                    cx.push_focus(
                        semantic_id.clone(),
                        LocalRect::new(0, 0, area.width, item_height),
                    );
                    cx.push_semantic(SemanticRegion::new(
                        semantic_id,
                        Rect::new(0, 0, area.width, item_height),
                        "list-item",
                    ));
                    cx.push_damage(LocalRect::new(0, 0, area.width, item_height));
                },
            );
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
        let pointer = match event {
            Event::Mouse(mouse) if !area.contains(mouse.position) => return EventOutcome::Ignored,
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
            let item_height = u16::try_from(measured.height).unwrap_or(u16::MAX);
            let item_area = translated_item_area(area, local_y, item_height);
            if pointer.is_some_and(|point| !item_area.contains(point)) {
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

fn translated_item_area(area: Rect, local_y: i64, item_height: u16) -> Rect {
    let top = i64::from(area.y).saturating_add(local_y);
    let bottom = top.saturating_add(i64::from(item_height));
    let visible_top = top.max(i64::from(area.y));
    let visible_bottom = bottom.min(i64::from(area.bottom()));
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
    use bmux_tui::composition::TextContent;
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
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b line that wraps"))
            .item("c", 0, TextContent::new("c"));
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
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b"))
            .item("c", 0, TextContent::new("c"))
            .item("d", 0, TextContent::new("d"));
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
            VirtualList::new("messages").item("a", 0, TextContent::new("a message that wraps"));
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
    fn state_geometry_queries_hide_index_implementation() {
        let list = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b wraps here"));
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
            list = list.item(key, 0, TextContent::new("x"));
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
    fn stable_viewport_survives_complete_keyed_mutation_sequence() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b wraps across rows"))
            .item("c", 0, TextContent::new("c wraps across rows"))
            .item("d", 0, TextContent::new("d"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state
            .scroll
            .set_vertical_offset(state.item_offset(&"c").unwrap().saturating_add(1));
        state.capture_anchor();

        let appended = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b wraps across rows"))
            .item("c", 0, TextContent::new("c wraps across rows"))
            .item("d", 0, TextContent::new("d"))
            .item("e", 0, TextContent::new("e"));
        assert_anchor_after_sync(&appended, 8, &mut state, "c", 1);

        let inserted = VirtualList::new("messages")
            .item("new", 0, TextContent::new("inserted above"))
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b wraps across rows"))
            .item("c", 0, TextContent::new("c wraps across rows"))
            .item("d", 0, TextContent::new("d"))
            .item("e", 0, TextContent::new("e"));
        assert_anchor_after_sync(&inserted, 8, &mut state, "c", 1);

        let reordered = VirtualList::new("messages")
            .item("e", 0, TextContent::new("e"))
            .item("d", 0, TextContent::new("d"))
            .item("c", 0, TextContent::new("c wraps across rows"))
            .item("b", 0, TextContent::new("b wraps across rows"))
            .item("a", 0, TextContent::new("a"))
            .item("new", 0, TextContent::new("inserted above"));
        assert_anchor_after_sync(&reordered, 8, &mut state, "c", 1);
        assert_anchor_after_sync(&reordered, 6, &mut state, "c", 1);

        let removed_above = VirtualList::new("messages")
            .item("e", 0, TextContent::new("e"))
            .item("c", 0, TextContent::new("c wraps across rows"))
            .item("b", 0, TextContent::new("b wraps across rows"))
            .item("a", 0, TextContent::new("a"))
            .item("new", 0, TextContent::new("inserted above"));
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
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b"))
            .item("c", 0, TextContent::new("c"));
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
    fn bottom_follow_survives_append_without_jump() {
        let initial = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b"))
            .item("c", 0, TextContent::new("c"));
        let mut state = VirtualListState::new(1);
        initial.sync(8, &mut state, &mut LayoutCx::new());
        state.scroll.set_follow_bottom(true);
        state.restore_anchor(2);
        assert_eq!(state.scroll.vertical_offset(), 3);

        let appended = VirtualList::new("messages")
            .item("a", 0, TextContent::new("a"))
            .item("b", 0, TextContent::new("b"))
            .item("c", 0, TextContent::new("c"))
            .item("d", 0, TextContent::new("d message wraps"));
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
