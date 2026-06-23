//! Shared hit-test primitives for component interaction regions.

use bmux_tui::geometry::{Point, Rect};

/// Generic rectangular hit-test region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitRegion<K> {
    /// Caller-defined region identifier.
    pub key: K,
    /// Rectangular terminal area covered by this region.
    pub rect: Rect,
}

impl<K> HitRegion<K> {
    /// Create a hit-test region.
    #[must_use]
    pub const fn new(key: K, rect: Rect) -> Self {
        Self { key, rect }
    }

    /// Return true when this region contains `point`.
    #[must_use]
    pub const fn contains(&self, point: Point) -> bool {
        self.rect.contains(point)
    }
}

/// Return the first region containing `point`.
#[must_use]
pub fn hit_region_at<K>(regions: &[HitRegion<K>], point: Point) -> Option<&HitRegion<K>> {
    regions.iter().find(|region| region.contains(point))
}

/// Build one hit region per row-like item with caller-provided heights.
#[must_use]
pub fn vertical_hit_regions(
    area: Rect,
    start_key: usize,
    heights: impl IntoIterator<Item = usize>,
) -> Vec<HitRegion<usize>> {
    let mut y = area.y;
    let bottom = area.bottom();
    let mut regions = Vec::new();
    for (offset, height) in heights.into_iter().enumerate() {
        if y >= bottom {
            break;
        }
        let height = u16::try_from(height).unwrap_or(u16::MAX);
        let visible_height = height.min(bottom.saturating_sub(y));
        if visible_height > 0 {
            regions.push(HitRegion::new(
                start_key.saturating_add(offset),
                Rect::new(area.x, y, area.width, visible_height),
            ));
        }
        y = y.saturating_add(height);
    }
    regions
}

#[cfg(test)]
mod tests {
    use bmux_tui::geometry::{Point, Rect};

    use super::{HitRegion, hit_region_at, vertical_hit_regions};

    #[test]
    fn finds_first_region_containing_point() {
        let regions = [
            HitRegion::new("a", Rect::new(0, 0, 2, 1)),
            HitRegion::new("b", Rect::new(2, 0, 2, 1)),
        ];

        assert_eq!(
            hit_region_at(&regions, Point::new(2, 0)).map(|r| r.key),
            Some("b")
        );
        assert_eq!(hit_region_at(&regions, Point::new(4, 0)), None);
    }

    #[test]
    fn builds_visible_vertical_regions() {
        let regions = vertical_hit_regions(Rect::new(0, 2, 10, 3), 4, [1, 2, 2]);

        assert_eq!(
            regions,
            vec![
                HitRegion::new(4, Rect::new(0, 2, 10, 1)),
                HitRegion::new(5, Rect::new(0, 3, 10, 2)),
            ]
        );
    }
}
