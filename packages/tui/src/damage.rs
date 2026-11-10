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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Damage {
    /// No terminal cells require presentation.
    None,
    /// Only these clipped terminal regions require presentation.
    Regions(Vec<Rect>),
    /// The content of `region` moved vertically by `rows` relative to the
    /// previously committed presentation, and `regions` require presentation.
    ///
    /// Positive `rows` move content toward row 0 (revealing rows at the
    /// bottom of `region`); negative `rows` move it away from row 0. The
    /// producer repaints every cell of `regions`, which always covers
    /// `region`, exactly as for [`Damage::Regions`]: preparation, retained
    /// cells and metadata treat this variant as regional damage of `regions`.
    /// The shift is only an output hint. A presenter may move the terminal's
    /// existing rows with a scroll region before writing the changed cells
    /// (see `ScrollRegionAcceleration` in [`crate::terminal`]); output is
    /// identical with or without that acceleration.
    ///
    /// Construct it with [`Damage::scroll`] so it is clipped, coalesced and
    /// bounded like region damage.
    Scroll {
        /// Rows whose content moved.
        region: Rect,
        /// Signed rows the content moved; never zero and smaller in
        /// magnitude than `region.height`.
        rows: i64,
        /// Every repainted region, merged and ordered; covers `region`.
        regions: Vec<Rect>,
    },
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
        let merged = merge_regions(regions, viewport);
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

    /// Construct bounded scroll damage: the content of `region` moved by
    /// `rows` and `region` plus `also` are repainted.
    ///
    /// `region` and `also` are clipped to `viewport`. The result degrades
    /// explicitly:
    ///
    /// * an empty clipped `region` yields [`Damage::regions`] of `also`;
    /// * `rows == 0`, or a shift of at least the region height (no row
    ///   survives), yields [`Damage::regions`] of `region` and `also`;
    /// * more merged regions than [`DamagePolicy::max_regions`], or newly
    ///   exposed rows plus repainted area outside `region` reaching
    ///   [`DamagePolicy::max_area_percent`] of the viewport, yields
    ///   [`Damage::Full`].
    ///
    /// The scrolled region itself does not count toward the area bound: rows
    /// that only moved are the point of scroll damage.
    #[must_use]
    pub fn scroll(
        region: Rect,
        rows: i64,
        also: impl IntoIterator<Item = Rect>,
        viewport: Rect,
        policy: DamagePolicy,
    ) -> Self {
        let region = region.intersection(viewport);
        if region.is_empty() {
            return Self::regions(also, viewport, policy);
        }
        if rows == 0 || rows.unsigned_abs() >= u64::from(region.height) {
            return Self::regions(std::iter::once(region).chain(also), viewport, policy);
        }
        let regions = merge_regions(std::iter::once(region).chain(also), viewport);
        let viewport_area = area(viewport);
        let outside = regions
            .iter()
            .fold(0_u64, |total, merged| total.saturating_add(area(*merged)))
            .saturating_sub(area(region));
        let exposed = rows
            .unsigned_abs()
            .saturating_mul(u64::from(region.width))
            .saturating_add(outside);
        if regions.len() > policy.max_regions
            || exposed.saturating_mul(100) / viewport_area >= u64::from(policy.max_area_percent)
        {
            return Self::Full;
        }
        Self::Scroll {
            region,
            rows,
            regions,
        }
    }

    /// Coalesce damage accumulated for the same future presentation.
    ///
    /// `None` is the identity and `Full` absorbs everything. Regions merge
    /// like [`Damage::regions`]. Scroll damage keeps its shift when combined
    /// with region damage, and two scrolls of the same region add their
    /// shifts (cancelling shifts become plain region damage). Scrolls of
    /// different regions cannot be expressed as one shift and degrade to
    /// region damage of everything either repaints. Every result is bounded
    /// by `policy` against `viewport`.
    ///
    /// Merging is safe in any order because scroll damage repaints its whole
    /// region; the shift only lets a presenter reuse rows already on screen.
    #[must_use]
    pub fn merge(self, other: Self, viewport: Rect, policy: DamagePolicy) -> Self {
        match (self, other) {
            (Self::Full, _) | (_, Self::Full) => Self::Full,
            (Self::None, other) | (other, Self::None) => other,
            (Self::Regions(left), Self::Regions(right)) => {
                Self::regions(left.into_iter().chain(right), viewport, policy)
            }
            (
                Self::Scroll {
                    region,
                    rows,
                    regions,
                },
                Self::Regions(extra),
            )
            | (
                Self::Regions(extra),
                Self::Scroll {
                    region,
                    rows,
                    regions,
                },
            ) => Self::scroll(
                region,
                rows,
                regions.into_iter().chain(extra),
                viewport,
                policy,
            ),
            (
                Self::Scroll {
                    region: left_region,
                    rows: left_rows,
                    regions: left,
                },
                Self::Scroll {
                    region: right_region,
                    rows: right_rows,
                    regions: right,
                },
            ) => {
                if left_region == right_region {
                    Self::scroll(
                        left_region,
                        left_rows.saturating_add(right_rows),
                        left.into_iter().chain(right),
                        viewport,
                        policy,
                    )
                } else {
                    Self::regions(left.into_iter().chain(right), viewport, policy)
                }
            }
        }
    }

    /// Return the repainted regions of region or scroll damage.
    #[must_use]
    pub fn retained_regions(&self) -> &[Rect] {
        match self {
            Self::Regions(regions) | Self::Scroll { regions, .. } => regions,
            Self::None | Self::Full => &[],
        }
    }

    /// Return the scrolled region and its signed row shift, for scroll damage.
    #[must_use]
    pub const fn scroll_shift(&self) -> Option<(Rect, i64)> {
        match self {
            Self::Scroll { region, rows, .. } => Some((*region, *rows)),
            Self::None | Self::Regions(_) | Self::Full => None,
        }
    }

    /// Return whether this damage repaints only some regions (region or scroll
    /// damage), retaining committed content and metadata outside them.
    #[must_use]
    pub const fn is_regional(&self) -> bool {
        matches!(self, Self::Regions(_) | Self::Scroll { .. })
    }

    /// Replace the repainted regions of regional damage, keeping a scroll
    /// shift when the new regions still cover its region. Other damage is
    /// returned unchanged.
    #[must_use]
    pub(crate) fn with_regions(self, regions: Vec<Rect>) -> Self {
        match self {
            Self::Regions(_) => Self::Regions(regions),
            Self::Scroll { region, rows, .. } => {
                if regions
                    .iter()
                    .any(|merged| merged.intersection(region) == region)
                {
                    Self::Scroll {
                        region,
                        rows,
                        regions,
                    }
                } else {
                    Self::Regions(regions)
                }
            }
            other @ (Self::None | Self::Full) => other,
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

/// Clip, merge touching or overlapping regions, and order them stably.
fn merge_regions(regions: impl IntoIterator<Item = Rect>, viewport: Rect) -> Vec<Rect> {
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
    merged
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

    #[test]
    fn scroll_damage_keeps_shift_and_covers_region() {
        let viewport = Rect::new(0, 0, 20, 10);
        let damage = Damage::scroll(
            Rect::new(0, 0, 20, 8),
            2,
            [Rect::new(0, 9, 5, 1)],
            viewport,
            DamagePolicy::default(),
        );
        assert_eq!(
            damage,
            Damage::Scroll {
                region: Rect::new(0, 0, 20, 8),
                rows: 2,
                regions: vec![Rect::new(0, 0, 20, 8), Rect::new(0, 9, 5, 1)],
            }
        );
        assert!(damage.is_regional());
        assert_eq!(damage.scroll_shift(), Some((Rect::new(0, 0, 20, 8), 2)));
        assert_eq!(damage.retained_regions().len(), 2);
    }

    #[test]
    fn scroll_damage_degrades_explicitly() {
        let viewport = Rect::new(0, 0, 20, 10);
        let policy = DamagePolicy::default();
        let region = Rect::new(0, 0, 20, 8);
        assert_eq!(
            Damage::scroll(region, 0, [], viewport, policy),
            Damage::regions([region], viewport, DamagePolicy::default())
        );
        assert!(Damage::scroll(region, 8, [], viewport, policy).is_full());
        assert!(Damage::scroll(region, -9, [], viewport, policy).is_full());
        assert!(
            Damage::scroll(Rect::new(30, 30, 2, 2), 1, [], viewport, policy).is_none(),
            "a region outside the viewport has nothing to scroll"
        );
        // Exposing most of the viewport costs as much as a full repaint.
        assert!(Damage::scroll(region, 7, [], viewport, policy).is_full());
        let count = Damage::scroll(
            region,
            1,
            [Rect::new(0, 9, 1, 1), Rect::new(5, 9, 1, 1)],
            viewport,
            DamagePolicy {
                max_regions: 2,
                max_area_percent: 100,
            },
        );
        assert!(count.is_full());
    }

    #[test]
    fn merge_coalesces_scrolls_and_regions() {
        let viewport = Rect::new(0, 0, 20, 10);
        let policy = DamagePolicy::default();
        let region = Rect::new(0, 0, 10, 8);
        let status = Rect::new(0, 9, 4, 1);
        let scroll = |rows| Damage::scroll(region, rows, [], viewport, policy);

        assert_eq!(Damage::None.merge(scroll(1), viewport, policy), scroll(1));
        assert!(Damage::Full.merge(scroll(1), viewport, policy).is_full());
        assert_eq!(
            Damage::Regions(vec![status]).merge(scroll(1), viewport, policy),
            Damage::scroll(region, 1, [status], viewport, policy)
        );
        assert_eq!(
            scroll(1).merge(scroll(2), viewport, policy),
            scroll(3),
            "shifts of one region add"
        );
        assert_eq!(
            scroll(2).merge(scroll(-2), viewport, policy),
            Damage::Regions(vec![region]),
            "cancelling shifts repaint without a shift"
        );
        let other = Damage::scroll(Rect::new(0, 0, 5, 8), 1, [], viewport, policy);
        assert_eq!(
            scroll(1).merge(other, viewport, policy),
            Damage::Regions(vec![region]),
            "scrolls of different regions degrade to region damage"
        );
    }
}
