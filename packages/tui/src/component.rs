//! Constraint-driven component layout primitives.
//!
//! Components resolve caller-supplied constraints into an authoritative layout
//! tree. Painting and interaction consume that tree instead of recomputing
//! geometry independently.
//!
//! Application and control state remains caller-owned and is supplied by the
//! concrete component value. The framework retains only derived geometry in
//! [`LayoutCache`]; cache entries are disposable and can always be recreated
//! from caller state, revisions, constraints, and [`LayoutEnvironment`].

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::event::{Event, EventOutcome};
use crate::geometry::{Rect, Size};
use crate::paint::PaintCx;

/// Root-relative rectangle in logical component coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LogicalRect {
    /// Horizontal origin in terminal cells.
    pub x: u16,
    /// Vertical origin in logical rows.
    pub y: usize,
    /// Width in terminal cells.
    pub width: u16,
    /// Height in logical rows.
    pub height: usize,
}

impl LogicalRect {
    /// Create a logical rectangle.
    #[must_use]
    pub const fn new(x: u16, y: usize, width: u16, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

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

    /// Deterministically combine a parent revision with one ordered child.
    /// Layout and paint channels remain independent.
    #[must_use]
    pub const fn combine(self, child: Self) -> Self {
        Self {
            layout: combine_revision(self.layout, child.layout),
            paint: combine_revision(self.paint, child.paint),
        }
    }
}

const fn combine_revision(parent: u64, child: u64) -> u64 {
    parent
        .wrapping_mul(0x9e37_79b1_85eb_ca87)
        .rotate_left(17)
        .wrapping_add(child)
        .wrapping_add(0x517c_c1b7_2722_0a95)
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

/// Additional component-owned metadata attached to authoritative layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutMetadata {
    /// Stable semantic labels/roles consumed by accessibility and inspection
    /// layers without reconstructing component geometry.
    pub semantics: Vec<String>,
}

impl LayoutMetadata {
    /// Create empty metadata.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            semantics: Vec::new(),
        }
    }

    /// Append one semantic label or role.
    #[must_use]
    pub fn semantic(mut self, value: impl Into<String>) -> Self {
        self.semantics.push(value.into());
        self
    }
}

/// Authoritative resolved geometry for a component and its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutNode {
    /// Stable node identity.
    pub id: LayoutId,
    /// Resolved logical size.
    pub size: LogicalSize,
    /// Component-owned semantic metadata.
    pub metadata: LayoutMetadata,
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
            metadata: LayoutMetadata::new(),
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
        Self {
            id,
            size,
            metadata: LayoutMetadata::new(),
            children,
        }
    }

    /// Attach component-owned metadata to this resolved node.
    #[must_use]
    pub fn with_metadata(mut self, metadata: LayoutMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Find one resolved node by stable layout identity without recalculating
    /// child placement or component measurement.
    #[must_use]
    pub fn find(&self, id: &LayoutId) -> Option<&Self> {
        if &self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.node.find(id))
    }

    /// Find one resolved node and its root-relative logical rectangle without
    /// recalculating component placement or saturating document coordinates.
    #[must_use]
    pub fn find_logical_rect(&self, id: &LayoutId) -> Option<LogicalRect> {
        self.find_logical_rect_at(id, 0, 0)
    }

    fn find_logical_rect_at(&self, id: &LayoutId, x: u16, y: usize) -> Option<LogicalRect> {
        if &self.id == id {
            return Some(LogicalRect::new(x, y, self.size.width, self.size.height));
        }
        self.children.iter().find_map(|child| {
            child.node.find_logical_rect_at(
                id,
                x.saturating_add(child.x),
                y.saturating_add(child.y),
            )
        })
    }

    /// Find one resolved node and its root-relative terminal rectangle without
    /// recalculating component placement.
    ///
    /// Prefer [`Self::find_logical_rect`] for scroll/document geometry; this
    /// terminal projection saturates logical rows at the `u16` boundary.
    #[must_use]
    pub fn find_rect(&self, id: &LayoutId) -> Option<Rect> {
        self.find_rect_at(id, 0, 0)
    }

    fn find_rect_at(&self, id: &LayoutId, x: u16, y: usize) -> Option<Rect> {
        if &self.id == id {
            return Some(self.terminal_rect(x, u16::try_from(y).unwrap_or(u16::MAX)));
        }
        self.children.iter().find_map(|child| {
            child
                .node
                .find_rect_at(id, x.saturating_add(child.x), y.saturating_add(child.y))
        })
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

/// Read-only services available while routing events through authoritative
/// resolved layout.
pub struct EventCx<'a> {
    root: &'a LayoutNode,
    translation_x: i32,
    translation_y: i64,
    logical_x: u16,
    logical_y: usize,
    clip: Option<Rect>,
}

impl<'a> EventCx<'a> {
    /// Create an event context from the exact resolved tree used for painting.
    #[must_use]
    pub const fn new(root: &'a LayoutNode) -> Self {
        Self {
            root,
            translation_x: 0,
            translation_y: 0,
            logical_x: 0,
            logical_y: 0,
            clip: None,
        }
    }

    /// Create an event context with a terminal-space clip for one presented tree.
    #[must_use]
    pub const fn with_clip(root: &'a LayoutNode, clip: Rect) -> Self {
        Self {
            root,
            translation_x: 0,
            translation_y: 0,
            logical_x: 0,
            logical_y: 0,
            clip: Some(clip),
        }
    }

    /// Complete authoritative tree for this event pass.
    #[must_use]
    pub const fn root(&self) -> &'a LayoutNode {
        self.root
    }

    /// Look up resolved component metadata/geometry by stable identity.
    #[must_use]
    pub fn find(&self, id: &LayoutId) -> Option<&'a LayoutNode> {
        self.root.find(id)
    }

    /// Look up root-relative logical geometry by stable identity.
    #[must_use]
    pub fn find_logical_rect(&self, id: &LayoutId) -> Option<LogicalRect> {
        self.root.find_logical_rect(id)
    }

    /// Current terminal-space clip, when event routing is viewport-scoped.
    #[must_use]
    pub const fn clip(&self) -> Option<Rect> {
        self.clip
    }

    /// Route a transformed child event with a terminal-space clip.
    pub fn with_transform<R>(
        &mut self,
        logical_x: u16,
        logical_y: usize,
        dx: i32,
        dy: i64,
        clip: Rect,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let old_x = self.translation_x;
        let old_y = self.translation_y;
        let old_logical_x = self.logical_x;
        let old_logical_y = self.logical_y;
        let old_clip = self.clip;
        self.translation_x = self.translation_x.saturating_add(dx);
        self.translation_y = self.translation_y.saturating_add(dy);
        self.logical_x = self.logical_x.saturating_add(logical_x);
        self.logical_y = self.logical_y.saturating_add(logical_y);
        self.clip = Some(old_clip.map_or(clip, |parent| parent.intersection(clip)));
        let result = f(self);
        self.translation_x = old_x;
        self.translation_y = old_y;
        self.logical_x = old_logical_x;
        self.logical_y = old_logical_y;
        self.clip = old_clip;
        result
    }

    /// Route against a separately retained subtree while preserving the current
    /// terminal transform and clip. The subtree becomes the geometry root for
    /// the duration of the callback.
    pub fn with_root<R>(&mut self, root: &LayoutNode, f: impl FnOnce(&mut EventCx<'_>) -> R) -> R {
        let mut nested = EventCx {
            root,
            translation_x: self.translation_x,
            translation_y: self.translation_y,
            logical_x: 0,
            logical_y: 0,
            clip: self.clip,
        };
        f(&mut nested)
    }

    /// Look up translated, clipped terminal geometry by stable identity.
    #[must_use]
    pub fn find_visible_rect(&self, id: &LayoutId) -> Option<Rect> {
        let logical = self.find_logical_rect(id)?;
        let local = LogicalRect::new(
            logical.x.saturating_sub(self.logical_x),
            logical.y.saturating_sub(self.logical_y),
            logical.width,
            logical.height,
        );
        let rect = translate_logical_rect(local, self.translation_x, self.translation_y);
        Some(self.clip.map_or(rect, |clip| clip.intersection(rect)))
    }

    /// Look up root-relative terminal geometry by stable identity.
    #[must_use]
    pub fn find_rect(&self, id: &LayoutId) -> Option<Rect> {
        self.find_visible_rect(id)
    }
}

fn translate_logical_rect(rect: LogicalRect, dx: i32, dy: i64) -> Rect {
    let x = i64::from(rect.x).saturating_add(i64::from(dx));
    let y = i64::try_from(rect.y).unwrap_or(i64::MAX).saturating_add(dy);
    let left = x.clamp(0, i64::from(u16::MAX));
    let top = y.clamp(0, i64::from(u16::MAX));
    let right = x
        .saturating_add(i64::from(rect.width))
        .clamp(0, i64::from(u16::MAX));
    let bottom = y
        .saturating_add(i64::try_from(rect.height).unwrap_or(i64::MAX))
        .clamp(0, i64::from(u16::MAX));
    Rect::new(
        u16::try_from(left).unwrap_or(0),
        u16::try_from(top).unwrap_or(0),
        u16::try_from(right.saturating_sub(left)).unwrap_or(u16::MAX),
        u16::try_from(bottom.saturating_sub(top)).unwrap_or(u16::MAX),
    )
}

/// A measurable, paintable, and event-participating terminal component.
pub trait Component {
    /// Resolve this component under explicit constraints.
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode;

    /// Paint this component from its authoritative resolved layout.
    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>);

    /// Route one event using the authoritative resolved layout. Caller-owned
    /// component/control state remains outside the framework cache.
    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }

    /// Caller-owned revisions used by retained layout and paint systems.
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::default()
    }
}

/// Geometry-affecting inputs supplied by the presentation environment.
///
/// Consumers increment `capability_revision` when a terminal capability that
/// can change measurement changes. Paint-only capability changes do not belong
/// here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayoutEnvironment {
    /// Revision of terminal capabilities that affect component geometry.
    pub capability_revision: u64,
}

impl LayoutEnvironment {
    /// Create layout environment inputs.
    #[must_use]
    pub const fn new(capability_revision: u64) -> Self {
        Self {
            capability_revision,
        }
    }
}

/// Cache key for one exact component measurement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LayoutCacheKey {
    id: LayoutId,
    layout_revision: u64,
    constraints: Constraints,
    environment: LayoutEnvironment,
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

    /// Resolve one component in the default geometry environment, reusing an
    /// exact retained layout when possible.
    pub fn layout(
        &mut self,
        id: LayoutId,
        component: &dyn Component,
        constraints: Constraints,
        cx: &mut LayoutCx,
    ) -> LayoutNode {
        self.layout_with_revision_and_environment(
            id,
            component,
            component.revision().layout,
            constraints,
            LayoutEnvironment::default(),
            cx,
        )
    }

    /// Resolve one component with an explicit caller-owned layout revision in
    /// the default geometry environment.
    pub fn layout_with_revision(
        &mut self,
        id: LayoutId,
        component: &dyn Component,
        layout_revision: u64,
        constraints: Constraints,
        cx: &mut LayoutCx,
    ) -> LayoutNode {
        self.layout_with_revision_and_environment(
            id,
            component,
            layout_revision,
            constraints,
            LayoutEnvironment::default(),
            cx,
        )
    }

    /// Resolve one component with explicit geometry-affecting environment
    /// inputs, reusing an exact retained layout when possible.
    pub fn layout_with_environment(
        &mut self,
        id: LayoutId,
        component: &dyn Component,
        constraints: Constraints,
        environment: LayoutEnvironment,
        cx: &mut LayoutCx,
    ) -> LayoutNode {
        self.layout_with_revision_and_environment(
            id,
            component,
            component.revision().layout,
            constraints,
            environment,
            cx,
        )
    }

    /// Resolve one component with explicit caller-owned revision and
    /// geometry-affecting environment inputs.
    pub fn layout_with_revision_and_environment(
        &mut self,
        id: LayoutId,
        component: &dyn Component,
        layout_revision: u64,
        constraints: Constraints,
        environment: LayoutEnvironment,
        cx: &mut LayoutCx,
    ) -> LayoutNode {
        let key = LayoutCacheKey {
            id,
            layout_revision,
            constraints,
            environment,
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

    /// Borrow one exact retained layout from the default environment.
    #[must_use]
    pub fn get(
        &self,
        id: &LayoutId,
        layout_revision: u64,
        constraints: Constraints,
    ) -> Option<&LayoutNode> {
        self.get_with_environment(
            id,
            layout_revision,
            constraints,
            LayoutEnvironment::default(),
        )
    }

    /// Borrow one exact retained layout with explicit geometry-affecting
    /// environment inputs.
    #[must_use]
    pub fn get_with_environment(
        &self,
        id: &LayoutId,
        layout_revision: u64,
        constraints: Constraints,
        environment: LayoutEnvironment,
    ) -> Option<&LayoutNode> {
        self.entries.get(&LayoutCacheKey {
            id: id.clone(),
            layout_revision,
            constraints,
            environment,
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

    /// Route one event through this erased component.
    pub fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.component.event(event, layout, cx)
    }

    /// Return caller-owned revisions.
    #[must_use]
    pub fn revision(&self) -> ComponentRevision {
        self.component.revision()
    }
}

impl Component for Element<'_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        Element::layout(self, constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        Element::paint(self, layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        Element::event(self, event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        Element::revision(self)
    }
}

/// Fold ordered child revisions into a parent revision so descendant changes
/// invalidate retained parent layout/paint state.
#[must_use]
pub fn combine_child_revisions(
    parent: ComponentRevision,
    children: impl IntoIterator<Item = ComponentRevision>,
) -> ComponentRevision {
    children
        .into_iter()
        .fold(parent, ComponentRevision::combine)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        Component, ComponentRevision, Constraints, Element, EventCx, EventOutcome, LayoutCache,
        LayoutCx, LayoutEnvironment, LayoutId, LayoutNode, LogicalSize,
    };
    use crate::event::Event;
    use crate::geometry::{Rect, Size};
    use crate::paint::PaintCx;

    #[test]
    fn event_protocol_consumes_authoritative_layout_without_remeasurement() {
        struct InteractiveLeaf;

        impl Component for InteractiveLeaf {
            fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
                cx.record_measurement();
                LayoutNode::leaf(
                    LayoutId::new("interactive"),
                    constraints.constrain(LogicalSize::new(1, 1)),
                )
                .with_metadata(super::LayoutMetadata::new().semantic("button"))
            }

            fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

            fn event(
                &self,
                event: &Event,
                layout: &LayoutNode,
                cx: &mut EventCx<'_>,
            ) -> EventOutcome {
                if matches!(event, Event::User(value) if value == "activate")
                    && cx.find_visible_rect(&layout.id) == Some(Rect::new(4, 3, 1, 1))
                {
                    EventOutcome::Handled
                } else {
                    EventOutcome::Ignored
                }
            }
        }

        let component = InteractiveLeaf;
        let mut layout_cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(1), &mut layout_cx);
        let measured = layout_cx.measured_nodes();
        let mut event_cx = EventCx::with_clip(&layout, Rect::new(0, 0, 10, 10));
        let outcome = event_cx.with_transform(0, 0, 4, 3, Rect::new(0, 0, 10, 10), |cx| {
            component.event(&Event::User("activate".to_owned()), &layout, cx)
        });

        assert_eq!(outcome, EventOutcome::Handled);
        assert!(outcome.is_handled());
        assert_eq!(layout_cx.measured_nodes(), measured);
    }

    #[test]
    fn event_context_clips_translated_logical_geometry() {
        let child = LayoutNode::leaf(LayoutId::new("child"), LogicalSize::new(4, 3));
        let root = LayoutNode::with_children(
            LayoutId::new("root"),
            LogicalSize::new(8, 6),
            vec![super::ChildLayout::new(2, 4, child)],
        );
        let mut cx = EventCx::with_clip(&root, Rect::new(0, 0, 8, 2));
        let visible = cx.with_transform(2, 4, 2, 1, Rect::new(0, 0, 8, 2), |cx| {
            cx.find_visible_rect(&LayoutId::new("child"))
        });

        assert_eq!(visible, Some(Rect::new(2, 1, 4, 1)));
        assert_eq!(
            cx.find_rect(&LayoutId::new("child")),
            Some(Rect::new(2, 4, 4, 0))
        );
    }

    #[test]
    fn authoritative_tree_carries_metadata_and_supports_identity_lookup() {
        let child = LayoutNode::leaf(LayoutId::new("child"), LogicalSize::new(2, 1))
            .with_metadata(super::LayoutMetadata::new().semantic("button"));
        let root = LayoutNode::with_children(
            LayoutId::new("root"),
            LogicalSize::new(2, 1),
            vec![super::ChildLayout::new(0, 0, child)],
        );

        let found = root.find(&LayoutId::new("child")).unwrap();
        assert_eq!(found.metadata.semantics, ["button"]);
        assert_eq!(
            root.find_rect(&LayoutId::new("child")),
            Some(crate::geometry::Rect::new(0, 0, 2, 1))
        );
        assert!(root.find(&LayoutId::new("missing")).is_none());
    }

    #[test]
    fn logical_lookup_preserves_document_coordinates_above_terminal_limits() {
        let child = LayoutNode::leaf(LayoutId::new("child"), LogicalSize::new(2, 70_000));
        let root = LayoutNode::with_children(
            LayoutId::new("root"),
            LogicalSize::new(2, 80_000),
            vec![super::ChildLayout::new(0, 70_000, child)],
        );

        assert_eq!(
            root.find_logical_rect(&LayoutId::new("child")),
            Some(super::LogicalRect::new(0, 70_000, 2, 70_000))
        );
        assert_eq!(
            root.find_rect(&LayoutId::new("child")),
            Some(crate::geometry::Rect::new(0, u16::MAX, 2, u16::MAX))
        );
        assert_eq!(
            EventCx::new(&root).find_logical_rect(&LayoutId::new("child")),
            Some(super::LogicalRect::new(0, 70_000, 2, 70_000))
        );
    }

    #[test]
    fn keyed_child_identity_survives_reorder_in_authoritative_tree() {
        let first = crate::composition::Column::new()
            .child(crate::composition::TextBlock::new("a").id("a"))
            .child(crate::composition::TextBlock::new("b").id("b"));
        let reordered = crate::composition::Column::new()
            .child(crate::composition::TextBlock::new("b").id("b"))
            .child(crate::composition::TextBlock::new("a").id("a"));
        let constraints = Constraints::for_width(4);
        let first = first.layout(constraints, &mut LayoutCx::new());
        let reordered = reordered.layout(constraints, &mut LayoutCx::new());

        assert_eq!(first.children[0].node.id.as_str(), "a");
        assert_eq!(first.children[1].node.id.as_str(), "b");
        assert_eq!(reordered.children[0].node.id.as_str(), "b");
        assert_eq!(reordered.children[1].node.id.as_str(), "a");
    }

    #[test]
    fn deep_nested_layout_is_deterministic_and_zero_size_safe() {
        let mut component = Element::new(TestComponent {
            revision: ComponentRevision::default(),
        });
        for depth in 0..512 {
            component = Element::new(OneChild {
                id: LayoutId::new(format!("node-{depth}")),
                child: component,
            });
        }
        let constraints = Constraints::new(0, 0, 0, Some(0));
        let first = component.layout(constraints, &mut LayoutCx::new());
        let second = component.layout(constraints, &mut LayoutCx::new());

        assert_eq!(first, second);
        assert_eq!(first.size, LogicalSize::default());
        let mut depth = 0;
        let mut node = &first;
        while let Some(child) = node.children.first() {
            depth += 1;
            node = &child.node;
        }
        assert_eq!(depth, 512);
    }

    struct OneChild<'a> {
        id: LayoutId,
        child: Element<'a>,
    }

    impl Component for OneChild<'_> {
        fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
            cx.record_measurement();
            let child = self.child.layout(constraints, cx);
            LayoutNode::with_children(
                self.id.clone(),
                constraints.constrain(child.size),
                vec![super::ChildLayout::new(0, 0, child)],
            )
        }

        fn paint(&self, _layout: &LayoutNode, _cx: &mut PaintCx<'_, '_>) {}

        fn revision(&self) -> ComponentRevision {
            self.child.revision()
        }
    }

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
    fn explicit_caller_revision_invalidates_entries() {
        let mut cache = LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::for_width(8);
        let component = TestComponent {
            revision: ComponentRevision::default(),
        };
        for revision in [1, 1, 2] {
            cache.layout_with_revision(
                LayoutId::new("item"),
                &component,
                revision,
                constraints,
                &mut cx,
            );
        }

        assert_eq!(cx.measured_nodes(), 2);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 2);
    }

    #[test]
    fn geometry_capability_revision_invalidates_entries() {
        let mut cache = LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::for_width(8);
        let component = TestComponent {
            revision: ComponentRevision::new(1, 0),
        };
        for capability_revision in [1, 1, 2] {
            cache.layout_with_environment(
                LayoutId::new("item"),
                &component,
                constraints,
                LayoutEnvironment::new(capability_revision),
                &mut cx,
            );
        }

        assert_eq!(cx.measured_nodes(), 2);
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 2);
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
