//! Shared neutral primitives for higher-level TUI components.

use bmux_tui::component::LogicalSize;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::style::Style;

/// Runtime interaction flags common to interactive controls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[allow(clippy::struct_excessive_bools)]
pub struct InteractionState {
    /// Control currently has keyboard focus.
    pub focused: bool,
    /// Pointer is currently over the control's active area.
    pub hovered: bool,
    /// Primary pointer/button activation is currently held.
    pub pressed: bool,
    /// Control is disabled and should ignore activation input.
    pub disabled: bool,
}

impl InteractionState {
    /// Create enabled interaction state with no focus/hover/press flags set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            focused: false,
            hovered: false,
            pressed: false,
            disabled: false,
        }
    }

    /// Return state marked as focused.
    #[must_use]
    pub const fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Return state marked as disabled.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Styles resolved from common interaction flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionStyles {
    /// Base/default style.
    pub normal: Style,
    /// Style for focused controls.
    pub focused: Style,
    /// Style for hovered controls.
    pub hovered: Style,
    /// Style for pressed controls.
    pub pressed: Style,
    /// Style for disabled controls.
    pub disabled: Style,
}

impl InteractionStyles {
    /// Create a style set.
    #[must_use]
    pub const fn new(
        normal: Style,
        focused: Style,
        hovered: Style,
        pressed: Style,
        disabled: Style,
    ) -> Self {
        Self {
            normal,
            focused,
            hovered,
            pressed,
            disabled,
        }
    }

    /// Resolve style from interaction state.
    ///
    /// Precedence is disabled > pressed > focused > hovered > normal.
    #[must_use]
    pub const fn resolve(self, state: InteractionState) -> Style {
        if state.disabled {
            self.disabled
        } else if state.pressed {
            self.pressed
        } else if state.focused {
            self.focused
        } else if state.hovered {
            self.hovered
        } else {
            self.normal
        }
    }
}

/// Reusable mouse behavior policy for simple controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentMousePolicy {
    /// Whether the control accepts mouse events at all.
    pub enabled: bool,
    /// Whether pointer movement should update hover state.
    pub hover: bool,
    /// Whether primary-button clicks activate the control.
    pub click: bool,
}

impl ComponentMousePolicy {
    /// Mouse handling disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            hover: false,
            click: false,
        }
    }

    /// Common button-like mouse behavior.
    #[must_use]
    pub const fn button() -> Self {
        Self {
            enabled: true,
            hover: true,
            click: true,
        }
    }
}

impl Default for ComponentMousePolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Runtime pointer drag state for controls that opt into dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragState {
    /// Initial pointer position where dragging started.
    pub origin: Point,
    /// Most recent pointer position.
    pub current: Point,
}

impl DragState {
    /// Start a drag at `origin`.
    #[must_use]
    pub const fn new(origin: Point) -> Self {
        Self {
            origin,
            current: origin,
        }
    }

    /// Return updated drag state.
    #[must_use]
    pub const fn moved_to(mut self, current: Point) -> Self {
        self.current = current;
        self
    }

    /// Return signed terminal-cell delta from origin to current.
    #[must_use]
    pub const fn delta(self) -> (i32, i32) {
        (
            self.current.x as i32 - self.origin.x as i32,
            self.current.y as i32 - self.origin.y as i32,
        )
    }
}

/// A stable identifier associated with a rectangular hit region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitRegionId(pub u64);

/// Reusable rectangular hit-region primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentHitRegion {
    /// Caller-chosen stable region id.
    pub id: HitRegionId,
    /// Region area in terminal cells.
    pub area: bmux_tui::geometry::Rect,
}

impl ComponentHitRegion {
    /// Create a hit region.
    #[must_use]
    pub const fn new(id: HitRegionId, area: bmux_tui::geometry::Rect) -> Self {
        Self { id, area }
    }

    /// Return true when `point` is inside this region.
    #[must_use]
    pub const fn contains(self, point: Point) -> bool {
        self.area.contains(point)
    }
}

/// Return the id of the first hit region containing `point`.
#[must_use]
pub fn hit_region_at(regions: &[ComponentHitRegion], point: Point) -> Option<HitRegionId> {
    regions
        .iter()
        .find_map(|region| region.contains(point).then_some(region.id))
}

/// Reusable resize bounds primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeBounds {
    /// Minimum size.
    pub min: bmux_tui::geometry::Size,
    /// Optional maximum size.
    pub max: Option<bmux_tui::geometry::Size>,
}

impl ResizeBounds {
    /// Create resize bounds.
    #[must_use]
    pub const fn new(min: bmux_tui::geometry::Size, max: Option<bmux_tui::geometry::Size>) -> Self {
        Self { min, max }
    }

    /// Clamp a size into these bounds.
    #[must_use]
    pub const fn clamp(self, size: bmux_tui::geometry::Size) -> bmux_tui::geometry::Size {
        let mut width = if size.width < self.min.width {
            self.min.width
        } else {
            size.width
        };
        let mut height = if size.height < self.min.height {
            self.min.height
        } else {
            size.height
        };
        if let Some(max) = self.max {
            if width > max.width {
                width = max.width;
            }
            if height > max.height {
                height = max.height;
            }
        }
        bmux_tui::geometry::Size::new(width, height)
    }
}

/// Saturate a logical row/cell count into terminal-space `u16`.
///
/// Logical geometry is unbounded; this is the single conversion used where a
/// component projects a logical size onto a terminal rectangle.
#[must_use]
pub fn u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// The local-coordinate rectangle covering a resolved logical size.
#[must_use]
pub fn local_area_of(size: LogicalSize) -> Rect {
    Rect::new(0, 0, size.width, u16_saturating(size.height))
}

/// Translate a terminal-space rectangle nested inside `area` into
/// `area`-relative local coordinates.
#[must_use]
pub const fn local_rect(area: Rect, inner: Rect) -> Rect {
    Rect::new(
        inner.x.saturating_sub(area.x),
        inner.y.saturating_sub(area.y),
        inner.width,
        inner.height,
    )
}

#[cfg(test)]
mod tests {
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::style::{Color, Style};

    use super::{
        ComponentHitRegion, HitRegionId, InteractionState, InteractionStyles, ResizeBounds,
        hit_region_at,
    };

    #[test]
    fn hit_region_at_returns_first_containing_region() {
        let regions = [
            ComponentHitRegion::new(HitRegionId(1), Rect::new(0, 0, 4, 4)),
            ComponentHitRegion::new(HitRegionId(2), Rect::new(2, 2, 4, 4)),
        ];

        assert_eq!(
            hit_region_at(&regions, Point::new(3, 3)),
            Some(HitRegionId(1))
        );
        assert_eq!(
            hit_region_at(&regions, Point::new(5, 5)),
            Some(HitRegionId(2))
        );
        assert_eq!(hit_region_at(&regions, Point::new(9, 9)), None);
    }

    #[test]
    fn interaction_styles_apply_explicit_precedence() {
        let styles = InteractionStyles::new(
            Style::new().fg(Color::White),
            Style::new().fg(Color::Green),
            Style::new().fg(Color::Yellow),
            Style::new().fg(Color::Blue),
            Style::new().fg(Color::Red),
        );

        assert_eq!(
            styles.resolve(InteractionState::new().focused(true)),
            Style::new().fg(Color::Green)
        );
        assert_eq!(
            styles.resolve(InteractionState {
                hovered: true,
                pressed: true,
                ..InteractionState::new()
            }),
            Style::new().fg(Color::Blue)
        );
        assert_eq!(
            styles.resolve(InteractionState {
                focused: true,
                disabled: true,
                ..InteractionState::new()
            }),
            Style::new().fg(Color::Red)
        );
    }

    #[test]
    fn resize_bounds_clamps_minimum_and_maximum_size() {
        let bounds = ResizeBounds::new(Size::new(3, 4), Some(Size::new(10, 12)));

        assert_eq!(bounds.clamp(Size::new(1, 20)), Size::new(3, 12));
    }
}
