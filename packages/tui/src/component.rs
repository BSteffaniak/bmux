//! Constraint-driven component layout primitives.
//!
//! Components resolve caller-supplied constraints into an authoritative layout
//! tree. Painting and interaction consume that tree instead of recomputing
//! geometry independently.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::geometry::{Rect, Size};
use crate::paint::PaintCx;

/// Logical component size.
///
/// Width is bounded by terminal cell coordinates. Height remains logical so a
/// scrollable document can exceed the visible terminal coordinate range.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LogicalSize {
    /// Width in terminal cells.
    pub width: u16,
    /// Height in logical terminal rows.
    pub height: usize,
}

impl LogicalSize {
    /// Create a logical size.
    #[must_use]
    pub const fn new(width: u16, height: usize) -> Self {
        Self { width, height }
    }

    /// Create a terminal-sized logical size.
    #[must_use]
    pub const fn terminal(size: Size) -> Self {
        Self::new(size.width, size.height as usize)
    }
}

/// Normalized minimum and maximum dimensions supplied to component layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Constraints {
    min_width: u16,
    max_width: u16,
    min_height: usize,
    max_height: Option<usize>,
}

impl Constraints {
    /// Create normalized constraints. Minimum dimensions are clamped to finite
    /// maxima when a caller supplies an inverted range.
    #[must_use]
    pub const fn new(
        min_width: u16,
        max_width: u16,
        min_height: usize,
        max_height: Option<usize>,
    ) -> Self {
        let min_width = if min_width > max_width {
            max_width
        } else {
            min_width
        };
        let min_height = match max_height {
            Some(maximum) if min_height > maximum => maximum,
            _ => min_height,
        };
        Self {
            min_width,
            max_width,
            min_height,
            max_height,
        }
    }

    /// Require exactly one terminal size.
    #[must_use]
    pub const fn tight(size: Size) -> Self {
        Self::new(
            size.width,
            size.width,
            size.height as usize,
            Some(size.height as usize),
        )
    }

    /// Permit any height at one exact width.
    #[must_use]
    pub const fn for_width(width: u16) -> Self {
        Self::new(width, width, 0, None)
    }

    /// Permit any size up to a terminal size.
    #[must_use]
    pub const fn loose(size: Size) -> Self {
        Self::new(0, size.width, 0, Some(size.height as usize))
    }

    /// Minimum width.
    #[must_use]
    pub const fn min_width(self) -> u16 {
        self.min_width
    }

    /// Maximum width.
    #[must_use]
    pub const fn max_width(self) -> u16 {
        self.max_width
    }

    /// Minimum logical height.
    #[must_use]
    pub const fn min_height(self) -> usize {
        self.min_height
    }

    /// Maximum logical height, or `None` when height is unbounded.
    #[must_use]
    pub const fn max_height(self) -> Option<usize> {
        self.max_height
    }

    /// Clamp a proposed size to these constraints.
    #[must_use]
    pub fn constrain(self, size: LogicalSize) -> LogicalSize {
        let width = size.width.clamp(self.min_width, self.max_width);
        let height = self.max_height.map_or_else(
            || size.height.max(self.min_height),
            |maximum| size.height.clamp(self.min_height, maximum),
        );
        LogicalSize::new(width, height)
    }

    /// Return constraints for content inside fixed terminal-cell insets.
    #[must_use]
    pub const fn inset(self, horizontal: u16, vertical: usize) -> Self {
        Self::new(
            self.min_width.saturating_sub(horizontal),
            self.max_width.saturating_sub(horizontal),
            self.min_height.saturating_sub(vertical),
            match self.max_height {
                Some(height) => Some(height.saturating_sub(vertical)),
                None => None,
            },
        )
    }
}

/// Stable caller-owned identity for one component layout node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayoutId(Cow<'static, str>);

impl LayoutId {
    /// Create a layout identity.
    #[must_use]
    pub fn new(id: impl Into<Cow<'static, str>>) -> Self {
        Self(id.into())
    }

    /// Return the identity text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&'static str> for LayoutId {
    fn from(value: &'static str) -> Self {
        Self::new(value)
    }
}

impl From<String> for LayoutId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Independent caller-owned revisions for geometry and paint output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ComponentRevision {
    /// Increment when measurement or child placement can change.
    pub layout: u64,
    /// Increment when paint output can change without changing geometry.
    pub paint: u64,
}

impl ComponentRevision {
    /// Create component revisions.
    #[must_use]
    pub const fn new(layout: u64, paint: u64) -> Self {
        Self { layout, paint }
    }
}

/// Placement and resolved layout for one child node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLayout {
    /// Child origin relative to its parent.
    pub x: u16,
    /// Logical child row relative to its parent.
    pub y: usize,
    /// Child layout.
    pub node: LayoutNode,
}

impl ChildLayout {
    /// Create a child placement.
    #[must_use]
    pub const fn new(x: u16, y: usize, node: LayoutNode) -> Self {
        Self { x, y, node }
    }
}

/// Authoritative resolved geometry for a component and its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutNode {
    /// Stable node identity.
    pub id: LayoutId,
    /// Resolved logical size.
    pub size: LogicalSize,
    /// Child placements in deterministic paint order.
    pub children: Vec<ChildLayout>,
}

impl LayoutNode {
    /// Create a leaf layout node.
    #[must_use]
    pub const fn leaf(id: LayoutId, size: LogicalSize) -> Self {
        Self {
            id,
            size,
            children: Vec::new(),
        }
    }

    /// Create a layout node with children.
    #[must_use]
    pub const fn with_children(
        id: LayoutId,
        size: LogicalSize,
        children: Vec<ChildLayout>,
    ) -> Self {
        Self { id, size, children }
    }

    /// Return a visible terminal rectangle at an assigned origin, saturating
    /// logical height only at the terminal-coordinate boundary.
    #[must_use]
    pub fn terminal_rect(&self, x: u16, y: u16) -> Rect {
        Rect::new(
            x,
            y,
            self.size.width,
            u16::try_from(self.size.height).unwrap_or(u16::MAX),
        )
    }
}

/// Mutable services available while resolving component layout.
#[derive(Debug, Default)]
pub struct LayoutCx {
    measured_nodes: usize,
}

impl LayoutCx {
    /// Create a layout context.
    #[must_use]
    pub const fn new() -> Self {
        Self { measured_nodes: 0 }
    }

    /// Record one component measurement.
    pub const fn record_measurement(&mut self) {
        self.measured_nodes = self.measured_nodes.saturating_add(1);
    }

    /// Number of component measurements performed through this context.
    #[must_use]
    pub const fn measured_nodes(&self) -> usize {
        self.measured_nodes
    }
}

/// A measurable and paintable terminal component.
pub trait Component {
    /// Resolve this component under explicit constraints.
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode;

    /// Paint this component from its authoritative resolved layout.
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>);

    /// Caller-owned revisions used by retained layout and paint systems.
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::default()
    }
}

/// Cache key for one exact component measurement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LayoutCacheKey {
    id: LayoutId,
    layout_revision: u64,
    constraints: Constraints,
}

/// Structural counters for retained layout behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutCacheStats {
    /// Exact cache matches.
    pub hits: usize,
    /// Measurements required because no exact entry existed.
    pub misses: usize,
    /// Entries released by explicit retention.
    pub released: usize,
}

/// Framework-owned retained layouts keyed by identity, layout revision, and constraints.
#[derive(Debug, Default)]
pub struct LayoutCache {
    entries: BTreeMap<LayoutCacheKey, LayoutNode>,
    stats: LayoutCacheStats,
}

impl LayoutCache {
    /// Create an empty retained layout cache.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            stats: LayoutCacheStats {
                hits: 0,
                misses: 0,
                released: 0,
            },
        }
    }

    /// Resolve one component, reusing exact retained geometry when possible.
    pub fn layout(
        &mut self,
        id: LayoutId,
        component: &dyn Component,
        constraints: Constraints,
        cx: &mut LayoutCx,
    ) -> LayoutNode {
        let key = LayoutCacheKey {
            id,
            layout_revision: component.revision().layout,
            constraints,
        };
        if let Some(node) = self.entries.get(&key) {
            self.stats.hits = self.stats.hits.saturating_add(1);
            return node.clone();
        }
        self.stats.misses = self.stats.misses.saturating_add(1);
        let node = component.layout(constraints, cx);
        self.entries.insert(key, node.clone());
        node
    }

    /// Borrow one exact retained layout.
    #[must_use]
    pub fn get(
        &self,
        id: &LayoutId,
        layout_revision: u64,
        constraints: Constraints,
    ) -> Option<&LayoutNode> {
        self.entries.get(&LayoutCacheKey {
            id: id.clone(),
            layout_revision,
            constraints,
        })
    }

    /// Retain only entries whose stable layout identities remain active.
    pub fn retain_ids(&mut self, active: &BTreeSet<LayoutId>) {
        let before = self.entries.len();
        self.entries.retain(|key, _| active.contains(&key.id));
        self.stats.released = self
            .stats
            .released
            .saturating_add(before.saturating_sub(self.entries.len()));
    }

    /// Remove every retained layout.
    pub fn clear(&mut self) {
        self.stats.released = self.stats.released.saturating_add(self.entries.len());
        self.entries.clear();
    }

    /// Number of retained exact measurements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no layouts are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Current structural counters.
    #[must_use]
    pub const fn stats(&self) -> LayoutCacheStats {
        self.stats
    }
}

/// Borrow-friendly type erasure for heterogeneous component trees.
pub struct Element<'a> {
    component: Box<dyn Component + 'a>,
}

impl<'a> Element<'a> {
    /// Erase a concrete component.
    #[must_use]
    pub fn new(component: impl Component + 'a) -> Self {
        Self {
            component: Box::new(component),
        }
    }

    /// Borrow the erased component protocol.
    #[must_use]
    pub fn as_component(&self) -> &dyn Component {
        self.component.as_ref()
    }

    /// Resolve this component under explicit constraints.
    pub fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.component.layout(constraints, cx)
    }

    /// Paint this component from a resolved layout.
    pub fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.component.paint(layout, cx);
    }

    /// Return caller-owned revisions.
    #[must_use]
    pub fn revision(&self) -> ComponentRevision {
        self.component.revision()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        Component, ComponentRevision, Constraints, LayoutCache, LayoutCx, LayoutId, LayoutNode,
        LogicalSize,
    };
    use crate::geometry::Size;
    use crate::paint::PaintCx;

    #[test]
    fn constraints_normalize_inverted_ranges_and_clamp_sizes() {
        let constraints = Constraints::new(10, 4, 8, Some(3));

        assert_eq!(constraints.min_width(), 4);
        assert_eq!(constraints.min_height(), 3);
        assert_eq!(
            constraints.constrain(LogicalSize::new(20, 20)),
            LogicalSize::new(4, 3)
        );
    }

    #[test]
    fn unbounded_height_remains_logical() {
        let constraints = Constraints::for_width(80);

        assert_eq!(
            constraints.constrain(LogicalSize::new(80, 100_000)),
            LogicalSize::new(80, 100_000)
        );
    }

    #[test]
    fn tight_constraints_require_terminal_size() {
        let constraints = Constraints::tight(Size::new(12, 4));

        assert_eq!(
            constraints.constrain(LogicalSize::new(1, 1)),
            LogicalSize::new(12, 4)
        );
    }

    struct TestComponent {
        revision: ComponentRevision,
    }

    impl Component for TestComponent {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            LayoutNode::leaf(
                LayoutId::new("resolved"),
                constraints.constrain(LogicalSize::new(4, 2)),
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn revision(&self) -> ComponentRevision {
            self.revision
        }
    }

    #[test]
    fn retained_layout_reuses_exact_geometry_and_ignores_paint_revision() {
        let mut cache = LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::for_width(8);
        let id = LayoutId::new("item");
        cache.layout(
            id.clone(),
            &TestComponent {
                revision: ComponentRevision::new(1, 1),
            },
            constraints,
            &mut cx,
        );
        cache.layout(
            id,
            &TestComponent {
                revision: ComponentRevision::new(1, 2),
            },
            constraints,
            &mut cx,
        );

        assert_eq!(cx.measured_nodes(), 1);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn layout_revision_width_and_removed_identity_invalidate_entries() {
        let mut cache = LayoutCache::new();
        let mut cx = LayoutCx::new();
        for (revision, width) in [(1, 8), (2, 8), (2, 10)] {
            cache.layout(
                LayoutId::new("item"),
                &TestComponent {
                    revision: ComponentRevision::new(revision, 0),
                },
                Constraints::for_width(width),
                &mut cx,
            );
        }
        assert_eq!(cx.measured_nodes(), 3);
        cache.retain_ids(&BTreeSet::new());
        assert!(cache.is_empty());
        assert_eq!(cache.stats().released, 3);
    }

    #[test]
    fn terminal_rect_saturates_only_at_render_boundary() {
        let node = LayoutNode::leaf(LayoutId::new("large"), LogicalSize::new(5, 100_000));

        assert_eq!(node.terminal_rect(2, 3).height, u16::MAX);
        assert_eq!(node.size.height, 100_000);
    }
}
