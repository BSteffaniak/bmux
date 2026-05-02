use crate::render::{DamageCoalescingPolicy, DamageRect, FrameDamage};
use bmux_attach_layout_protocol::{AttachLayer, AttachRect, AttachScene};
use bmux_plugin::RenderOp;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedOpacity {
    Opaque,
    Transparent,
    Unknown,
}

impl RetainedOpacity {
    #[must_use]
    pub const fn from_opaque(opaque: bool) -> Self {
        if opaque {
            Self::Opaque
        } else {
            Self::Transparent
        }
    }

    #[must_use]
    pub const fn is_opaque(self) -> bool {
        matches!(self, Self::Opaque)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetainedSurfacePayload {
    RenderOps(Vec<RenderOp>),
    Content { content_id: Uuid },
    Unknown,
}

impl RetainedSurfacePayload {
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedSurface {
    pub id: Uuid,
    pub rect: DamageRect,
    pub layer: i16,
    pub z: i32,
    pub opaque: bool,
    pub opacity: RetainedOpacity,
    pub clip_rect: Option<DamageRect>,
    pub interactive_regions: Vec<DamageRect>,
    pub payload: RetainedSurfacePayload,
}

impl RetainedSurface {
    #[must_use]
    pub const fn builder(id: Uuid, rect: DamageRect) -> RetainedSurfaceBuilder {
        RetainedSurfaceBuilder::new(id, rect)
    }

    #[must_use]
    pub const fn new(
        id: Uuid,
        rect: DamageRect,
        layer: i16,
        z: i32,
        opaque: bool,
        ops: Vec<RenderOp>,
    ) -> Self {
        Self::with_payload(
            id,
            rect,
            layer,
            z,
            RetainedOpacity::from_opaque(opaque),
            RetainedSurfacePayload::RenderOps(ops),
        )
    }

    #[must_use]
    pub const fn with_payload(
        id: Uuid,
        rect: DamageRect,
        layer: i16,
        z: i32,
        opacity: RetainedOpacity,
        payload: RetainedSurfacePayload,
    ) -> Self {
        Self {
            id,
            rect,
            layer,
            z,
            opaque: opacity.is_opaque(),
            opacity,
            clip_rect: None,
            interactive_regions: Vec::new(),
            payload,
        }
    }

    #[must_use]
    pub const fn with_clip_rect(mut self, clip_rect: Option<DamageRect>) -> Self {
        self.clip_rect = clip_rect;
        self
    }

    #[must_use]
    pub fn with_interactive_regions(mut self, interactive_regions: Vec<DamageRect>) -> Self {
        self.interactive_regions = interactive_regions;
        self
    }

    #[must_use]
    pub const fn z_key(&self) -> (i16, i32, Uuid) {
        (self.layer, self.z, self.id)
    }

    #[must_use]
    pub fn paint_rect(&self) -> Option<DamageRect> {
        self.clip_rect
            .map_or(Some(self.rect), |clip| intersect_rects(self.rect, clip))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedSurfaceBuilder {
    surface: RetainedSurface,
}

impl RetainedSurfaceBuilder {
    #[must_use]
    pub const fn new(id: Uuid, rect: DamageRect) -> Self {
        Self {
            surface: RetainedSurface::with_payload(
                id,
                rect,
                0,
                0,
                RetainedOpacity::Unknown,
                RetainedSurfacePayload::Unknown,
            ),
        }
    }

    #[must_use]
    pub const fn layer(mut self, layer: i16) -> Self {
        self.surface.layer = layer;
        self
    }

    #[must_use]
    pub const fn z(mut self, z: i32) -> Self {
        self.surface.z = z;
        self
    }

    #[must_use]
    pub const fn opacity(mut self, opacity: RetainedOpacity) -> Self {
        self.surface.opacity = opacity;
        self.surface.opaque = opacity.is_opaque();
        self
    }

    #[must_use]
    pub const fn opaque(self) -> Self {
        self.opacity(RetainedOpacity::Opaque)
    }

    #[must_use]
    pub const fn transparent(self) -> Self {
        self.opacity(RetainedOpacity::Transparent)
    }

    #[must_use]
    pub fn render_ops(mut self, ops: Vec<RenderOp>) -> Self {
        self.surface.payload = RetainedSurfacePayload::RenderOps(ops);
        self
    }

    #[must_use]
    pub fn content(mut self, content_id: Uuid) -> Self {
        self.surface.payload = RetainedSurfacePayload::Content { content_id };
        self
    }

    #[must_use]
    pub fn unknown_payload(mut self) -> Self {
        self.surface.payload = RetainedSurfacePayload::Unknown;
        self
    }

    #[must_use]
    pub const fn clip_rect(mut self, clip_rect: DamageRect) -> Self {
        self.surface.clip_rect = Some(clip_rect);
        self
    }

    #[must_use]
    pub fn interactive_region(mut self, region: DamageRect) -> Self {
        self.surface.interactive_regions.push(region);
        self
    }

    #[must_use]
    pub fn interactive_regions(mut self, regions: Vec<DamageRect>) -> Self {
        self.surface.interactive_regions = regions;
        self
    }

    #[must_use]
    pub fn build(self) -> RetainedSurface {
        self.surface
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

        let ordered = self.ordered_surfaces();
        let mut by_surface: BTreeMap<Uuid, RetainedRepaintSurface> = BTreeMap::new();
        for damage_rect in damage_rects {
            let intersecting = ordered
                .iter()
                .enumerate()
                .filter_map(|(index, surface)| {
                    let paint_rect = surface.paint_rect()?;
                    let damage = intersect_rects(damage_rect, paint_rect)?;
                    Some((index, *surface, damage))
                })
                .collect::<Vec<_>>();
            if intersecting.is_empty() {
                continue;
            }

            let start_index = intersecting
                .iter()
                .rev()
                .find_map(|(index, surface, _)| {
                    let paint_rect = surface.paint_rect()?;
                    if surface.opacity != RetainedOpacity::Opaque
                        || !rect_contains(paint_rect, damage_rect)
                    {
                        return None;
                    }
                    let earliest_transparent_underlay = intersecting
                        .iter()
                        .take_while(|(under_index, _, _)| under_index < index)
                        .filter(|(_, under_surface, _)| {
                            under_surface.opacity != RetainedOpacity::Opaque
                        })
                        .map(|(under_index, _, _)| *under_index)
                        .min();
                    Some(earliest_transparent_underlay.unwrap_or(*index))
                })
                .unwrap_or(0);

            for (index, surface, surface_damage) in intersecting {
                if index < start_index {
                    continue;
                }
                by_surface
                    .entry(surface.id)
                    .and_modify(|entry| entry.damage.push(surface_damage))
                    .or_insert_with(|| RetainedRepaintSurface {
                        surface_id: surface.id,
                        rect: surface.rect,
                        layer: surface.layer,
                        z: surface.z,
                        opaque: surface.opaque,
                        opacity: surface.opacity,
                        clip_rect: surface.clip_rect,
                        interactive_regions: surface.interactive_regions.clone(),
                        damage: vec![surface_damage],
                    });
            }
        }

        let mut plan = by_surface.into_values().collect::<Vec<_>>();
        plan.sort_by_key(|surface| (surface.layer, surface.z, surface.surface_id));
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
    pub opacity: RetainedOpacity,
    pub clip_rect: Option<DamageRect>,
    pub interactive_regions: Vec<DamageRect>,
    pub damage: Vec<DamageRect>,
}

#[must_use]
pub fn retained_frame_damage_from_frame_damage(
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    viewport: DamageRect,
    policy: DamageCoalescingPolicy,
) -> RetainedDamage {
    if frame_damage.is_full_frame() {
        return RetainedDamage::Full { viewport };
    }
    let mut rects = Vec::new();
    if frame_damage.status_damaged() {
        rects.extend(layer_rects(scene, AttachLayer::Status));
    }
    if frame_damage.overlay_damaged() {
        rects.extend(layer_rects(scene, AttachLayer::Overlay));
        rects.extend(layer_rects(scene, AttachLayer::FloatingPane));
        rects.extend(layer_rects(scene, AttachLayer::Tooltip));
    }

    for surface in scene.surfaces.iter().filter(|surface| surface.visible) {
        if frame_damage.extension_surfaces().contains(&surface.id) {
            rects.push(damage_rect_from_attach_rect(surface.rect));
        }
        for rect in frame_damage.extension_surface_rects(surface.id) {
            rects.push(translate_damage_rect(
                *rect,
                damage_rect_from_attach_rect(surface.rect),
            ));
        }
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if frame_damage.content_surfaces().contains(&pane_id) {
            rects.push(damage_rect_from_attach_rect(surface.content_rect));
        }
        for rect in frame_damage.content_surface_rects(pane_id) {
            rects.push(translate_damage_rect(
                *rect,
                damage_rect_from_attach_rect(surface.content_rect),
            ));
        }
    }
    coalesce_absolute_damage(rects, viewport, policy)
}

#[must_use]
pub fn retained_repaint_plan_from_frame_damage(
    scene: &AttachScene,
    frame_damage: &FrameDamage,
    viewport: DamageRect,
    policy: DamageCoalescingPolicy,
) -> Vec<RetainedRepaintSurface> {
    let mut compositor = RetainedCompositor::new();
    let damage = retained_frame_damage_from_frame_damage(scene, frame_damage, viewport, policy);
    let _ = compositor.replace_surfaces(
        retained_surfaces_from_attach_scene(scene),
        viewport,
        DamageCoalescingPolicy {
            max_rects: usize::MAX,
            max_area_percent: 100,
        },
    );
    compositor.repaint_plan(&damage)
}

#[must_use]
pub fn retained_surfaces_from_attach_scene(scene: &AttachScene) -> Vec<RetainedSurface> {
    scene
        .surfaces
        .iter()
        .filter(|surface| surface.visible)
        .map(|surface| {
            let payload = surface
                .pane_id
                .map_or(RetainedSurfacePayload::Unknown, |pane_id| {
                    RetainedSurfacePayload::Content {
                        content_id: pane_id,
                    }
                });
            RetainedSurface::with_payload(
                surface.id,
                damage_rect_from_attach_rect(surface.rect),
                retained_layer_order(surface.layer),
                surface.z,
                RetainedOpacity::from_opaque(surface.opaque),
                payload,
            )
            .with_interactive_regions(
                surface
                    .interactive_regions
                    .iter()
                    .map(|region| damage_rect_from_attach_rect(region.rect))
                    .collect(),
            )
        })
        .collect()
}

fn layer_rects(scene: &AttachScene, layer: AttachLayer) -> impl Iterator<Item = DamageRect> + '_ {
    scene
        .surfaces
        .iter()
        .filter(move |surface| surface.visible && surface.layer == layer)
        .map(|surface| damage_rect_from_attach_rect(surface.rect))
}

const fn translate_damage_rect(rect: DamageRect, origin: DamageRect) -> DamageRect {
    DamageRect::new(
        origin.x.saturating_add(rect.x),
        origin.y.saturating_add(rect.y),
        rect.w,
        rect.h,
    )
}

const fn retained_layer_order(layer: AttachLayer) -> i16 {
    match layer {
        AttachLayer::Status => 0,
        AttachLayer::Pane => 10,
        AttachLayer::Overlay => 20,
        AttachLayer::FloatingPane => 30,
        AttachLayer::Tooltip => 40,
        AttachLayer::Cursor => 50,
    }
}

const fn damage_rect_from_attach_rect(rect: AttachRect) -> DamageRect {
    DamageRect::new(rect.x, rect.y, rect.w, rect.h)
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

const fn rect_contains(outer: DamageRect, inner: DamageRect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.right() >= inner.right()
        && outer.bottom() >= inner.bottom()
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
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        InteractiveRegion,
    };
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
    fn retained_surface_builder_sets_payload_opacity_clip_and_regions() {
        let surface = RetainedSurface::builder(Uuid::from_u128(42), DamageRect::new(1, 2, 3, 4))
            .layer(5)
            .z(6)
            .opaque()
            .content(Uuid::from_u128(7))
            .clip_rect(DamageRect::new(1, 2, 2, 2))
            .interactive_region(DamageRect::new(1, 2, 1, 1))
            .build();

        assert_eq!(surface.id, Uuid::from_u128(42));
        assert_eq!(surface.layer, 5);
        assert_eq!(surface.z, 6);
        assert!(surface.opaque);
        assert_eq!(surface.opacity, RetainedOpacity::Opaque);
        assert_eq!(
            surface.payload,
            RetainedSurfacePayload::Content {
                content_id: Uuid::from_u128(7),
            }
        );
        assert_eq!(surface.paint_rect(), Some(DamageRect::new(1, 2, 2, 2)));
        assert_eq!(
            surface.interactive_regions,
            vec![DamageRect::new(1, 2, 1, 1)]
        );
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
        assert_eq!(plan[0].opacity, RetainedOpacity::Transparent);
        assert!(!plan[0].opaque);
        assert_eq!(plan[1].opacity, RetainedOpacity::Opaque);
        assert!(plan[1].opaque);
        assert_eq!(plan[0].damage, vec![DamageRect::new(2, 0, 2, 1)]);
    }

    #[test]
    fn repaint_plan_prunes_underlays_covered_by_opaque_surface() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            [
                surface(1, 0, 0, 0, 10, true),
                surface(2, 0, 0, 0, 20, true),
                surface(3, 1, 0, 0, 30, false),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        let plan =
            compositor.repaint_plan(&RetainedDamage::Regions(vec![DamageRect::new(1, 0, 2, 1)]));

        assert_eq!(
            plan.iter()
                .map(|surface| surface.surface_id)
                .collect::<Vec<_>>(),
            vec![Uuid::from_u128(2), Uuid::from_u128(3)]
        );
    }

    #[test]
    fn repaint_plan_respects_clip_rects_and_interactive_regions() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let mut compositor = RetainedCompositor::new();
        let clipped = RetainedSurface::with_payload(
            Uuid::from_u128(1),
            DamageRect::new(0, 0, 10, 5),
            0,
            0,
            RetainedOpacity::Unknown,
            RetainedSurfacePayload::Content {
                content_id: Uuid::from_u128(99),
            },
        )
        .with_clip_rect(Some(DamageRect::new(2, 0, 4, 5)))
        .with_interactive_regions(vec![DamageRect::new(2, 1, 2, 1)]);
        let _ = compositor.replace_surfaces([clipped], viewport, DamageCoalescingPolicy::default());

        let outside =
            compositor.repaint_plan(&RetainedDamage::Regions(vec![DamageRect::new(0, 0, 1, 1)]));
        assert!(outside.is_empty());

        let inside =
            compositor.repaint_plan(&RetainedDamage::Regions(vec![DamageRect::new(3, 1, 1, 1)]));
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].opacity, RetainedOpacity::Unknown);
        assert_eq!(inside[0].clip_rect, Some(DamageRect::new(2, 0, 4, 5)));
        assert_eq!(
            inside[0].interactive_regions,
            vec![DamageRect::new(2, 1, 2, 1)]
        );
    }

    #[test]
    fn retained_surfaces_from_attach_scene_maps_visible_scene_surfaces() {
        let pane_id = Uuid::from_u128(7);
        let hidden_id = Uuid::from_u128(3);
        let scene = AttachScene {
            session_id: Uuid::from_u128(1),
            focus: AttachFocusTarget::None,
            surfaces: vec![
                AttachSurface {
                    id: Uuid::from_u128(2),
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 4,
                    rect: AttachRect {
                        x: 1,
                        y: 2,
                        w: 10,
                        h: 5,
                    },
                    content_rect: AttachRect {
                        x: 2,
                        y: 3,
                        w: 8,
                        h: 3,
                    },
                    interactive_regions: vec![InteractiveRegion {
                        rect: AttachRect {
                            x: 1,
                            y: 2,
                            w: 10,
                            h: 1,
                        },
                        region_id: "title".to_owned(),
                        owning_plugin_id: "example".to_owned(),
                    }],
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: false,
                    pane_id: Some(pane_id),
                },
                AttachSurface {
                    id: hidden_id,
                    kind: AttachSurfaceKind::Overlay,
                    layer: AttachLayer::Overlay,
                    z: 5,
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                    },
                    content_rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                    },
                    interactive_regions: Vec::new(),
                    opaque: false,
                    visible: false,
                    accepts_input: false,
                    cursor_owner: false,
                    pane_id: None,
                },
            ],
        };

        let surfaces = retained_surfaces_from_attach_scene(&scene);

        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].id, Uuid::from_u128(2));
        assert_eq!(surfaces[0].layer, 10);
        assert_eq!(surfaces[0].z, 4);
        assert_eq!(surfaces[0].rect, DamageRect::new(1, 2, 10, 5));
        assert_eq!(surfaces[0].opacity, RetainedOpacity::Opaque);
        assert_eq!(
            surfaces[0].payload,
            RetainedSurfacePayload::Content {
                content_id: pane_id,
            }
        );
        assert_eq!(
            surfaces[0].interactive_regions,
            vec![DamageRect::new(1, 2, 10, 1)]
        );
    }

    #[test]
    fn retained_repaint_plan_from_frame_damage_translates_content_and_extension_rects() {
        let pane_id = Uuid::from_u128(70);
        let surface_id = Uuid::from_u128(71);
        let scene = AttachScene {
            session_id: Uuid::from_u128(1),
            focus: AttachFocusTarget::None,
            surfaces: vec![AttachSurface {
                id: surface_id,
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 10,
                    y: 5,
                    w: 20,
                    h: 10,
                },
                content_rect: AttachRect {
                    x: 12,
                    y: 7,
                    w: 16,
                    h: 6,
                },
                interactive_regions: Vec::new(),
                opaque: false,
                visible: true,
                accepts_input: true,
                cursor_owner: false,
                pane_id: Some(pane_id),
            }],
        };
        let mut frame_damage = FrameDamage::default();
        frame_damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(1, 2, 3, 1),
            (16, 6),
            DamageCoalescingPolicy::default(),
        );
        frame_damage.mark_extension_surface_rect(
            surface_id,
            DamageRect::new(0, 0, 2, 1),
            (20, 10),
            DamageCoalescingPolicy::default(),
        );

        let plan = retained_repaint_plan_from_frame_damage(
            &scene,
            &frame_damage,
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].damage,
            vec![DamageRect::new(10, 5, 2, 1), DamageRect::new(13, 9, 3, 1)]
        );
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
