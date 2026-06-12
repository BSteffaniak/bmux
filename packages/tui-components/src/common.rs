//! Shared neutral primitives for higher-level TUI components.

use bmux_tui::geometry::Point;

/// Runtime interaction flags common to interactive controls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
