//! Shared two-axis scrollbar layout helpers.

use bmux_tui::geometry::Rect;

/// Axis scrollbar layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollbarAxisLayoutMode {
    /// Axis scrollbar is hidden.
    Hidden,
    /// Axis scrollbar overlays content.
    Overlay,
    /// Axis scrollbar reserves gutter space.
    Gutter,
}

impl ScrollbarAxisLayoutMode {
    const fn enabled(self) -> bool {
        !matches!(self, Self::Hidden)
    }

    const fn gutter(self) -> bool {
        matches!(self, Self::Gutter)
    }
}

/// Two-axis scrollbar layout input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarLayoutPolicy {
    /// Vertical scrollbar layout mode.
    pub vertical: ScrollbarAxisLayoutMode,
    /// Horizontal scrollbar layout mode.
    pub horizontal: ScrollbarAxisLayoutMode,
}

impl ScrollbarLayoutPolicy {
    /// Create a layout policy.
    #[must_use]
    pub const fn new(
        vertical: ScrollbarAxisLayoutMode,
        horizontal: ScrollbarAxisLayoutMode,
    ) -> Self {
        Self {
            vertical,
            horizontal,
        }
    }
}

/// Resolved content and scrollbar areas for a two-axis viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarLayout {
    /// Content viewport area after gutter reservation.
    pub content: Rect,
    /// Vertical scrollbar area, when enabled.
    pub vertical_scrollbar: Option<Rect>,
    /// Horizontal scrollbar area, when enabled.
    pub horizontal_scrollbar: Option<Rect>,
    /// Bottom-right corner cell reserved when both gutter scrollbars are enabled.
    pub corner: Option<Rect>,
}

/// Compute content, scrollbar, and corner areas for a two-axis viewport.
#[must_use]
pub const fn scrollbar_layout(area: Rect, policy: ScrollbarLayoutPolicy) -> ScrollbarLayout {
    let reserve_vertical = policy.vertical.enabled() && policy.vertical.gutter() && area.width > 0;
    let reserve_horizontal =
        policy.horizontal.enabled() && policy.horizontal.gutter() && area.height > 0;
    let content = Rect::new(
        area.x,
        area.y,
        if reserve_vertical {
            area.width.saturating_sub(1)
        } else {
            area.width
        },
        if reserve_horizontal {
            area.height.saturating_sub(1)
        } else {
            area.height
        },
    );
    let vertical_scrollbar = if policy.vertical.enabled() && area.width > 0 {
        Some(Rect::new(
            area.right().saturating_sub(1),
            area.y,
            1,
            content.height,
        ))
    } else {
        None
    };
    let horizontal_scrollbar = if policy.horizontal.enabled() && area.height > 0 {
        Some(Rect::new(
            area.x,
            area.bottom().saturating_sub(1),
            content.width,
            1,
        ))
    } else {
        None
    };
    let corner = if reserve_vertical && reserve_horizontal {
        Some(Rect::new(
            area.right().saturating_sub(1),
            area.bottom().saturating_sub(1),
            1,
            1,
        ))
    } else {
        None
    };
    ScrollbarLayout {
        content,
        vertical_scrollbar,
        horizontal_scrollbar,
        corner,
    }
}
