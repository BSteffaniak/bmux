//! Lazy item sources for virtualized lists.

use bmux_tui::component::{Element, LayoutEnvironment, LayoutNode};

/// An ordered, indexable collection of keyed items whose components are
/// constructed on demand.
///
/// Source order equals index order. The list calls [`Self::build`] only to
/// measure an item that is new, has a changed layout revision, or is stale for
/// the current width or environment and has no [`Self::retained_layout`]; and
/// once per paint or event dispatch for each visible item (construction only,
/// the retained layout is reused). It is never called for other items.
///
/// [`Self::key`] and [`Self::layout_revision`] are read for items covered by
/// noted splices and invalidations, for visible items, and during a full
/// reconciliation. [`Self::paint_revision`] is read only for visible items.
///
/// Item keys must be unique, and their `ToString` representations must be
/// unique too, because they name retained layouts and interaction regions.
pub trait VirtualItemSource<K> {
    /// Number of items.
    fn len(&self) -> usize;

    /// Whether the source has no items.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stable key of the item at `index`.
    fn key(&self, index: usize) -> K;

    /// Caller-owned layout revision of the item at `index`. Change it whenever
    /// the item's measurement or internal placement can change.
    fn layout_revision(&self, index: usize) -> u64;

    /// Caller-owned paint revision of the item at `index`.
    fn paint_revision(&self, _index: usize) -> u64 {
        0
    }

    /// Construct the component for the item at `index`.
    fn build(&self, index: usize) -> Element<'_>;

    /// A caller-retained exact layout for this item at `width` and
    /// `environment`. When `Some`, the list uses it for measurement, painting,
    /// and events, and never calls [`Self::build`] to measure this item.
    fn retained_layout(
        &self,
        _index: usize,
        _width: u64,
        _environment: LayoutEnvironment,
    ) -> Option<&LayoutNode> {
        None
    }

    /// Whether every synchronization must compare all keys and revisions.
    ///
    /// Sources that report membership changes with
    /// `VirtualListState::note_splice` and revision changes with
    /// `VirtualListState::invalidate` return `false` (the default), so steady
    /// scrolling performs no `O(n)` work. Sources rebuilt from scratch for
    /// every frame, such as the eager [`super::VirtualList`] builder, return
    /// `true`.
    fn always_reconcile(&self) -> bool {
        false
    }
}

impl<K, T: VirtualItemSource<K> + ?Sized> VirtualItemSource<K> for &T {
    fn len(&self) -> usize {
        (**self).len()
    }

    fn is_empty(&self) -> bool {
        (**self).is_empty()
    }

    fn key(&self, index: usize) -> K {
        (**self).key(index)
    }

    fn layout_revision(&self, index: usize) -> u64 {
        (**self).layout_revision(index)
    }

    fn paint_revision(&self, index: usize) -> u64 {
        (**self).paint_revision(index)
    }

    fn build(&self, index: usize) -> Element<'_> {
        (**self).build(index)
    }

    fn retained_layout(
        &self,
        index: usize,
        width: u64,
        environment: LayoutEnvironment,
    ) -> Option<&LayoutNode> {
        (**self).retained_layout(index, width, environment)
    }

    fn always_reconcile(&self) -> bool {
        (**self).always_reconcile()
    }
}
