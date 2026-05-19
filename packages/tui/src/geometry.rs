//! Terminal-space geometry types.

/// A terminal-space point measured in cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    /// Zero-based column.
    pub x: u16,
    /// Zero-based row.
    pub y: u16,
}

impl Point {
    /// Create a point.
    #[must_use]
    pub const fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }
}

/// A terminal-space size measured in cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Size {
    /// Width in terminal columns.
    pub width: u16,
    /// Height in terminal rows.
    pub height: u16,
}

impl Size {
    /// Create a size.
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    /// Return true when either dimension is zero.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Insets from a rectangle edge, measured in terminal cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Insets {
    /// Top inset.
    pub top: u16,
    /// Right inset.
    pub right: u16,
    /// Bottom inset.
    pub bottom: u16,
    /// Left inset.
    pub left: u16,
}

impl Insets {
    /// Create edge insets.
    #[must_use]
    pub const fn new(top: u16, right: u16, bottom: u16, left: u16) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    /// Create the same inset for every edge.
    #[must_use]
    pub const fn all(value: u16) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    /// Combined horizontal inset.
    #[must_use]
    pub const fn horizontal(self) -> u16 {
        self.left.saturating_add(self.right)
    }

    /// Combined vertical inset.
    #[must_use]
    pub const fn vertical(self) -> u16 {
        self.top.saturating_add(self.bottom)
    }
}

/// A terminal-space rectangle measured in cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rect {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in terminal columns.
    pub width: u16,
    /// Height in terminal rows.
    pub height: u16,
}

impl Rect {
    /// Create a rectangle.
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Return the rectangle size.
    #[must_use]
    pub const fn size(self) -> Size {
        Size::new(self.width, self.height)
    }

    /// Return true when either dimension is zero.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Exclusive right edge.
    #[must_use]
    pub const fn right(self) -> u16 {
        self.x.saturating_add(self.width)
    }

    /// Exclusive bottom edge.
    #[must_use]
    pub const fn bottom(self) -> u16 {
        self.y.saturating_add(self.height)
    }

    /// Return true when the point is inside this rectangle.
    #[must_use]
    pub const fn contains(self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }

    /// Return this rectangle inset by the supplied edge distances.
    #[must_use]
    pub const fn inset(self, insets: Insets) -> Self {
        let x = self.x.saturating_add(insets.left);
        let y = self.y.saturating_add(insets.top);
        let width = self.width.saturating_sub(insets.horizontal());
        let height = self.height.saturating_sub(insets.vertical());
        Self::new(x, y, width, height)
    }

    /// Return the intersection of two rectangles.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        let x = max_u16(self.x, other.x);
        let y = max_u16(self.y, other.y);
        let right = min_u16(self.right(), other.right());
        let bottom = min_u16(self.bottom(), other.bottom());
        Self::new(x, y, right.saturating_sub(x), bottom.saturating_sub(y))
    }
}

const fn min_u16(a: u16, b: u16) -> u16 {
    if a < b { a } else { b }
}

const fn max_u16(a: u16, b: u16) -> u16 {
    if a > b { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::{Insets, Point, Rect, Size};

    #[test]
    fn rect_contains_uses_exclusive_edges() {
        let rect = Rect::new(2, 3, 4, 5);

        assert!(rect.contains(Point::new(2, 3)));
        assert!(rect.contains(Point::new(5, 7)));
        assert!(!rect.contains(Point::new(6, 7)));
        assert!(!rect.contains(Point::new(5, 8)));
    }

    #[test]
    fn rect_inset_saturates_to_empty_size() {
        let rect = Rect::new(1, 2, 3, 4).inset(Insets::all(4));

        assert_eq!(rect, Rect::new(5, 6, 0, 0));
    }

    #[test]
    fn rect_intersection_returns_overlap() {
        let left = Rect::new(1, 1, 4, 4);
        let right = Rect::new(3, 0, 4, 3);

        assert_eq!(left.intersection(right), Rect::new(3, 1, 2, 2));
    }

    #[test]
    fn size_empty_when_any_dimension_is_zero() {
        assert!(Size::new(0, 2).is_empty());
        assert!(Size::new(2, 0).is_empty());
        assert!(!Size::new(2, 2).is_empty());
    }
}
