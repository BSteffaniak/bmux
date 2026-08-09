//! Domain-neutral terminal presentation damage.

use crate::geometry::Rect;

/// Bounds used when coalescing damaged terminal regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamagePolicy {
    /// Maximum retained region count before promoting to a full presentation.
    pub max_regions: usize,
    /// Maximum damaged percentage of the viewport before promoting to full.
    pub max_area_percent: u16,
}

impl Default for DamagePolicy {
    fn default() -> Self {
        Self {
            max_regions: 64,
            max_area_percent: 60,
        }
    }
}

/// Coalesced damage for one process-local terminal presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    /// No terminal cells require presentation.
    None,
    /// Only these clipped terminal regions require presentation.
    Regions(Vec<Rect>),
    /// The complete viewport requires presentation.
    Full,
}

impl Damage {
    /// Construct bounded damage from terminal-space regions.
    #[must_use]
    pub fn regions(
        regions: impl IntoIterator<Item = Rect>,
        viewport: Rect,
        policy: DamagePolicy,
    ) -> Self {
        let mut merged = Vec::<Rect>::new();
        for region in regions {
            let mut next = region.intersection(viewport);
            if next.is_empty() {
                continue;
            }
            let mut index = 0;
            while index < merged.len() {
                if touches_or_overlaps(merged[index], next) {
                    next = union(merged.swap_remove(index), next);
                    index = 0;
                } else {
                    index += 1;
                }
            }
            merged.push(next);
        }
        merged.sort_by_key(|region| (region.y, region.x, region.height, region.width));
        if merged.is_empty() {
            return Self::None;
        }
        let viewport_area = area(viewport);
        if viewport_area == 0 {
            return Self::None;
        }
        let damaged_area = merged
            .iter()
            .fold(0_u64, |total, region| total.saturating_add(area(*region)));
        let damaged_percent = damaged_area.saturating_mul(100) / viewport_area;
        if merged.len() > policy.max_regions
            || damaged_percent >= u64::from(policy.max_area_percent)
        {
            Self::Full
        } else {
            Self::Regions(merged)
        }
    }

    /// Return the retained regions, if this is region damage.
    #[must_use]
    pub fn retained_regions(&self) -> &[Rect] {
        match self {
            Self::Regions(regions) => regions,
            Self::None | Self::Full => &[],
        }
    }

    /// Return whether no cells are damaged.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Return whether the complete viewport is damaged.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

fn area(rect: Rect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

const fn touches_or_overlaps(left: Rect, right: Rect) -> bool {
    left.x <= right.right()
        && right.x <= left.right()
        && left.y <= right.bottom()
        && right.y <= left.bottom()
}

const fn union(left: Rect, right: Rect) -> Rect {
    let x = if left.x < right.x { left.x } else { right.x };
    let y = if left.y < right.y { left.y } else { right.y };
    let right_edge = if left.right() > right.right() {
        left.right()
    } else {
        right.right()
    };
    let bottom = if left.bottom() > right.bottom() {
        left.bottom()
    } else {
        right.bottom()
    };
    Rect::new(x, y, right_edge.saturating_sub(x), bottom.saturating_sub(y))
}

#[cfg(test)]
mod tests {
    use super::{Damage, DamagePolicy};
    use crate::geometry::Rect;

    #[test]
    fn regions_are_clipped_merged_and_stably_ordered() {
        let damage = Damage::regions(
            [
                Rect::new(8, 3, 8, 3),
                Rect::new(1, 1, 2, 2),
                Rect::new(2, 2, 3, 1),
            ],
            Rect::new(0, 0, 10, 5),
            DamagePolicy::default(),
        );

        assert_eq!(
            damage,
            Damage::Regions(vec![Rect::new(1, 1, 4, 2), Rect::new(8, 3, 2, 2)])
        );
    }

    #[test]
    fn excessive_count_or_area_promotes_to_full() {
        let count = Damage::regions(
            [Rect::new(0, 0, 1, 1), Rect::new(3, 0, 1, 1)],
            Rect::new(0, 0, 10, 10),
            DamagePolicy {
                max_regions: 1,
                max_area_percent: 100,
            },
        );
        assert!(count.is_full());

        let area = Damage::regions(
            [Rect::new(0, 0, 6, 10)],
            Rect::new(0, 0, 10, 10),
            DamagePolicy::default(),
        );
        assert!(area.is_full());
    }

    #[test]
    fn empty_and_outside_regions_produce_no_damage() {
        let damage = Damage::regions(
            [Rect::new(20, 20, 2, 2), Rect::new(0, 0, 0, 2)],
            Rect::new(0, 0, 10, 10),
            DamagePolicy::default(),
        );
        assert!(damage.is_none());
    }
}
