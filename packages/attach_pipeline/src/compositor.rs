use crate::render::{DamageCoalescingPolicy, DamageRect};
use bmux_plugin::RenderOp;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedSurface {
    pub id: Uuid,
    pub rect: DamageRect,
    pub layer: i16,
    pub z: i32,
    pub opaque: bool,
    pub ops: Vec<RenderOp>,
}

impl RetainedSurface {
    #[must_use]
    pub const fn new(
        id: Uuid,
        rect: DamageRect,
        layer: i16,
        z: i32,
        opaque: bool,
        ops: Vec<RenderOp>,
    ) -> Self {
        Self {
            id,
            rect,
            layer,
            z,
            opaque,
            ops,
        }
    }

    #[must_use]
    pub const fn z_key(&self) -> (i16, i32, Uuid) {
        (self.layer, self.z, self.id)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetainedCompositor {
    surfaces: BTreeMap<Uuid, RetainedSurface>,
}

impl RetainedCompositor {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            surfaces: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn surfaces(&self) -> &BTreeMap<Uuid, RetainedSurface> {
        &self.surfaces
    }

    #[must_use]
    pub fn ordered_surfaces(&self) -> Vec<&RetainedSurface> {
        let mut surfaces = self.surfaces.values().collect::<Vec<_>>();
        surfaces.sort_by_key(|surface| surface.z_key());
        surfaces
    }

    #[must_use]
    pub fn repaint_plan(&self, damage: &RetainedDamage) -> Vec<RetainedRepaintSurface> {
        let damage_rects = match damage {
            RetainedDamage::None => return Vec::new(),
            RetainedDamage::Full { viewport } => vec![*viewport],
            RetainedDamage::Regions(rects) => rects.clone(),
        };

        let mut plan = Vec::new();
        for surface in self.ordered_surfaces() {
            let intersections = damage_rects
                .iter()
                .filter_map(|rect| intersect_rects(*rect, surface.rect))
                .collect::<Vec<_>>();
            if intersections.is_empty() {
                continue;
            }
            plan.push(RetainedRepaintSurface {
                surface_id: surface.id,
                rect: surface.rect,
                layer: surface.layer,
                z: surface.z,
                opaque: surface.opaque,
                damage: intersections,
            });
        }
        plan
    }

    pub fn replace_surfaces(
        &mut self,
        next_surfaces: impl IntoIterator<Item = RetainedSurface>,
        viewport: DamageRect,
        policy: DamageCoalescingPolicy,
    ) -> RetainedDamage {
        let previous = std::mem::take(&mut self.surfaces);
        let next = next_surfaces
            .into_iter()
            .map(|surface| (surface.id, surface))
            .collect::<BTreeMap<_, _>>();
        let damage = retained_scene_damage_between(&previous, &next, viewport, policy);
        self.surfaces = next;
        damage
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetainedDamage {
    None,
    Full { viewport: DamageRect },
    Regions(Vec<DamageRect>),
}

impl RetainedDamage {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }

    #[must_use]
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full { .. })
    }

    #[must_use]
    pub fn rects(&self) -> &[DamageRect] {
        match self {
            Self::None | Self::Full { .. } => &[],
            Self::Regions(rects) => rects,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedRepaintSurface {
    pub surface_id: Uuid,
    pub rect: DamageRect,
    pub layer: i16,
    pub z: i32,
    pub opaque: bool,
    pub damage: Vec<DamageRect>,
}

fn retained_scene_damage_between(
    previous: &BTreeMap<Uuid, RetainedSurface>,
    next: &BTreeMap<Uuid, RetainedSurface>,
    viewport: DamageRect,
    policy: DamageCoalescingPolicy,
) -> RetainedDamage {
    let mut damaged = Vec::new();
    let surface_ids = previous
        .keys()
        .chain(next.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for surface_id in surface_ids {
        match (previous.get(&surface_id), next.get(&surface_id)) {
            (Some(prev), Some(next)) if prev == next => {}
            (Some(prev), Some(next)) => {
                damaged.push(prev.rect);
                damaged.push(next.rect);
            }
            (Some(prev), None) => damaged.push(prev.rect),
            (None, Some(next)) => damaged.push(next.rect),
            (None, None) => {}
        }
    }
    coalesce_absolute_damage(damaged, viewport, policy)
}

fn coalesce_absolute_damage(
    rects: Vec<DamageRect>,
    viewport: DamageRect,
    policy: DamageCoalescingPolicy,
) -> RetainedDamage {
    let mut merged: Vec<DamageRect> = Vec::new();
    for rect in rects {
        let Some(mut next) = intersect_rects(rect, viewport) else {
            continue;
        };
        let mut index = 0;
        while index < merged.len() {
            if merged[index].touches_or_overlaps(next) {
                next = merged.swap_remove(index).union(next);
                index = 0;
            } else {
                index += 1;
            }
        }
        merged.push(next);
    }
    if merged.is_empty() {
        return RetainedDamage::None;
    }

    let viewport_area = viewport.area();
    if viewport_area == 0 {
        return RetainedDamage::None;
    }
    let damaged_area = merged
        .iter()
        .fold(0_u32, |area, rect| area.saturating_add(rect.area()));
    let area_percent = damaged_area.saturating_mul(100) / viewport_area;
    if merged.len() > policy.max_rects || area_percent >= u32::from(policy.max_area_percent) {
        RetainedDamage::Full { viewport }
    } else {
        RetainedDamage::Regions(merged)
    }
}

const fn intersect_rects(a: DamageRect, b: DamageRect) -> Option<DamageRect> {
    let x1 = if a.x > b.x { a.x } else { b.x };
    let y1 = if a.y > b.y { a.y } else { b.y };
    let x2 = if a.right() < b.right() {
        a.right()
    } else {
        b.right()
    };
    let y2 = if a.bottom() < b.bottom() {
        a.bottom()
    } else {
        b.bottom()
    };
    if x1 >= x2 || y1 >= y2 {
        None
    } else {
        Some(DamageRect::new(
            x1,
            y1,
            x2.saturating_sub(x1),
            y2.saturating_sub(y1),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{RenderOp, RenderStyle};

    fn surface(id: u128, x: u16, y: u16, layer: i16, z: i32, opaque: bool) -> RetainedSurface {
        RetainedSurface::new(
            Uuid::from_u128(id),
            DamageRect::new(x, y, 4, 2),
            layer,
            z,
            opaque,
            vec![RenderOp::TextRun {
                x,
                y,
                text: format!("surface-{id}"),
                style: RenderStyle::default(),
            }],
        )
    }

    #[test]
    fn replace_surfaces_damages_old_and_new_bounds_for_moves() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let mut compositor = RetainedCompositor::new();
        let initial = compositor.replace_surfaces(
            [surface(1, 1, 1, 0, 0, true)],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        assert_eq!(initial.rects(), &[DamageRect::new(1, 1, 4, 2)]);

        let moved = compositor.replace_surfaces(
            [surface(1, 10, 5, 0, 0, true)],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(
            moved.rects(),
            &[DamageRect::new(1, 1, 4, 2), DamageRect::new(10, 5, 4, 2)]
        );
    }

    #[test]
    fn repaint_plan_is_bottom_to_top_and_keeps_transparent_underlays() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            [
                surface(1, 0, 0, 0, 10, false),
                surface(2, 1, 0, 0, 20, true),
                surface(3, 20, 0, 0, 30, true),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        let plan =
            compositor.repaint_plan(&RetainedDamage::Regions(vec![DamageRect::new(2, 0, 2, 1)]));

        assert_eq!(
            plan.iter()
                .map(|surface| surface.surface_id)
                .collect::<Vec<_>>(),
            vec![Uuid::from_u128(1), Uuid::from_u128(2)]
        );
        assert!(!plan[0].opaque);
        assert!(plan[1].opaque);
        assert_eq!(plan[0].damage, vec![DamageRect::new(2, 0, 2, 1)]);
    }

    #[test]
    fn replace_surfaces_escalates_large_damage_to_full_viewport() {
        let viewport = DamageRect::new(0, 0, 10, 10);
        let mut compositor = RetainedCompositor::new();
        let damage = compositor.replace_surfaces(
            [RetainedSurface::new(
                Uuid::from_u128(1),
                DamageRect::new(0, 0, 10, 7),
                0,
                0,
                true,
                Vec::new(),
            )],
            viewport,
            DamageCoalescingPolicy {
                max_rects: 64,
                max_area_percent: 60,
            },
        );

        assert_eq!(damage, RetainedDamage::Full { viewport });
    }
}
