//! Generic rectangular hit testing for terminal UI surfaces.

use crate::event::{MouseEvent, MouseEventKind};
use crate::geometry::{Point, Rect};

/// Stable caller-owned identifier for a hit-test region.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HitId(String);

impl HitId {
    /// Create a hit-test identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for HitId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for HitId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Semantic hit-test behavior for a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HitRole {
    /// Generic clickable/actionable region.
    Action,
    /// Text entry/editing region.
    TextInput,
    /// Selectable list row or option.
    ListItem,
    /// Scrollable viewport/content region.
    Scroll,
    /// Resize handle.
    ResizeHandle,
    /// Draggable region.
    DragHandle,
    /// Non-actionable decoration/background region.
    Decoration,
    /// Caller-defined role code.
    Custom(u16),
}

/// One rectangular hit-test region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitRegion {
    /// Stable region id.
    pub id: HitId,
    /// Region bounds in terminal cells.
    pub area: Rect,
    /// Higher layers win over lower layers when regions overlap.
    pub layer: i16,
    /// Region role.
    pub role: HitRole,
    /// Whether this region accepts hover/move events.
    pub hoverable: bool,
}

impl HitRegion {
    /// Create a hit-test region.
    #[must_use]
    pub fn new(id: impl Into<HitId>, area: Rect) -> Self {
        Self {
            id: id.into(),
            area,
            layer: 0,
            role: HitRole::Action,
            hoverable: false,
        }
    }

    /// Set the region layer.
    #[must_use]
    pub const fn layer(mut self, layer: i16) -> Self {
        self.layer = layer;
        self
    }

    /// Set the region role.
    #[must_use]
    pub const fn role(mut self, role: HitRole) -> Self {
        self.role = role;
        self
    }

    /// Mark whether this region accepts hover/move events.
    #[must_use]
    pub const fn hoverable(mut self, hoverable: bool) -> Self {
        self.hoverable = hoverable;
        self
    }

    /// Return true when the region contains the point and is non-empty.
    #[must_use]
    pub const fn contains(&self, point: Point) -> bool {
        !self.area.is_empty() && self.area.contains(point)
    }

    /// Return true when this region should accept a mouse event kind.
    #[must_use]
    pub const fn accepts(&self, kind: MouseEventKind) -> bool {
        match kind {
            MouseEventKind::Move => self.hoverable,
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => true,
        }
    }
}

/// Hit-test match information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit<'region> {
    /// Matched region.
    pub region: &'region HitRegion,
    /// Point relative to the matched region origin.
    pub local: Point,
}

impl Hit<'_> {
    /// Return the matched region id.
    #[must_use]
    pub const fn id(&self) -> &HitId {
        &self.region.id
    }

    /// Return the matched region role.
    #[must_use]
    pub const fn role(&self) -> HitRole {
        self.region.role
    }
}

/// Ordered collection of rectangular hit-test regions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HitMap {
    regions: Vec<HitRegion>,
}

impl HitMap {
    /// Create an empty hit map.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    /// Remove all registered regions.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Add a hit-test region.
    pub fn push(&mut self, region: HitRegion) {
        if !region.area.is_empty() {
            self.regions.push(region);
        }
    }

    /// Add and return this hit map for builder-style composition.
    #[must_use]
    pub fn with_region(mut self, region: HitRegion) -> Self {
        self.push(region);
        self
    }

    /// Return all registered regions in insertion order.
    #[must_use]
    pub fn regions(&self) -> &[HitRegion] {
        &self.regions
    }

    /// Find the topmost region containing a point.
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<Hit<'_>> {
        self.regions
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, region)| region.contains(point))
            .max_by_key(|(index, region)| (region.layer, *index))
            .map(|(_, region)| Hit {
                region,
                local: Point::new(
                    point.x.saturating_sub(region.area.x),
                    point.y.saturating_sub(region.area.y),
                ),
            })
    }

    /// Find the topmost region accepting a mouse event.
    #[must_use]
    pub fn hit_mouse(&self, event: MouseEvent) -> Option<Hit<'_>> {
        self.regions
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, region)| region.contains(event.position) && region.accepts(event.kind))
            .max_by_key(|(index, region)| (region.layer, *index))
            .map(|(_, region)| Hit {
                region,
                local: Point::new(
                    event.position.x.saturating_sub(region.area.x),
                    event.position.y.saturating_sub(region.area.y),
                ),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{HitId, HitMap, HitRegion, HitRole};
    use crate::event::{MouseButton, MouseEvent, MouseEventKind};
    use crate::geometry::{Point, Rect};

    #[test]
    fn hit_map_returns_topmost_region_by_layer_then_insertion_order() {
        let map = HitMap::new()
            .with_region(HitRegion::new("bottom", Rect::new(0, 0, 10, 5)).layer(0))
            .with_region(HitRegion::new("top", Rect::new(2, 1, 5, 3)).layer(1))
            .with_region(HitRegion::new("latest", Rect::new(2, 1, 5, 3)).layer(1));

        let hit = map.hit_test(Point::new(3, 2)).expect("hit should resolve");

        assert_eq!(hit.id().as_str(), "latest");
        assert_eq!(hit.local, Point::new(1, 1));
    }

    #[test]
    fn hit_map_ignores_empty_regions_and_misses_outside_points() {
        let map = HitMap::new()
            .with_region(HitRegion::new("empty", Rect::new(0, 0, 0, 10)))
            .with_region(HitRegion::new("real", Rect::new(1, 1, 2, 2)));

        assert_eq!(map.regions().len(), 1);
        assert!(map.hit_test(Point::new(0, 0)).is_none());
        assert!(map.hit_test(Point::new(2, 2)).is_some());
    }

    #[test]
    fn hit_mouse_requires_hoverable_region_for_move_events() {
        let map = HitMap::new()
            .with_region(HitRegion::new("plain", Rect::new(0, 0, 5, 5)).layer(2))
            .with_region(
                HitRegion::new("hover", Rect::new(0, 0, 5, 5))
                    .layer(1)
                    .hoverable(true),
            );
        let event = MouseEvent::new(MouseEventKind::Move, Point::new(2, 2));

        let hit = map.hit_mouse(event).expect("hover region should match");

        assert_eq!(hit.id().as_str(), "hover");
    }

    #[test]
    fn hit_mouse_accepts_clicks_and_roles() {
        let map = HitMap::new().with_region(
            HitRegion::new(HitId::new("row-1"), Rect::new(4, 2, 10, 1)).role(HitRole::ListItem),
        );
        let event = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(6, 2));

        let hit = map.hit_mouse(event).expect("click should match row");

        assert_eq!(hit.id().as_str(), "row-1");
        assert_eq!(hit.role(), HitRole::ListItem);
        assert_eq!(hit.local, Point::new(2, 0));
    }
}
