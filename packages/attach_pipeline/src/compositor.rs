use crate::render::{DamageCoalescingPolicy, DamageRect, FrameDamage};
use bmux_attach_layout_protocol::{AttachLayer, AttachRect, AttachScene};
use bmux_plugin::surface::{PluginSurface, PluginSurfaceRegionId, PluginSurfaceTarget};
use bmux_plugin::{ExtensionRect, RenderOp, render_text_width_u16};
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
pub struct RetainedInteractiveRegion {
    pub id: Option<PluginSurfaceRegionId>,
    pub rect: DamageRect,
    pub focusable: bool,
    pub cursor: bmux_plugin::surface::PluginSurfaceCursor,
    pub endpoint: Option<bmux_plugin::AttachInputEndpoint>,
}

impl PartialEq<DamageRect> for RetainedInteractiveRegion {
    fn eq(&self, other: &DamageRect) -> bool {
        self.rect == *other
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedSurface {
    pub id: Uuid,
    pub rect: DamageRect,
    pub layer: i16,
    pub z: i32,
    pub opaque: bool,
    pub modal: bool,
    pub opacity: RetainedOpacity,
    pub clip_rect: Option<DamageRect>,
    pub revision: Option<u64>,
    pub interactive_regions: Vec<RetainedInteractiveRegion>,
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
            modal: false,
            opacity,
            clip_rect: None,
            revision: None,
            interactive_regions: Vec::new(),
            payload,
        }
    }

    #[must_use]
    pub const fn with_revision(mut self, revision: Option<u64>) -> Self {
        self.revision = revision;
        self
    }

    #[must_use]
    pub const fn with_clip_rect(mut self, clip_rect: Option<DamageRect>) -> Self {
        self.clip_rect = clip_rect;
        self
    }

    #[must_use]
    pub fn with_interactive_regions(mut self, interactive_regions: Vec<DamageRect>) -> Self {
        self.interactive_regions = interactive_regions
            .into_iter()
            .map(|rect| RetainedInteractiveRegion {
                id: None,
                rect,
                focusable: false,
                cursor: bmux_plugin::surface::PluginSurfaceCursor::Default,
                endpoint: None,
            })
            .collect();
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

#[must_use]
pub fn retained_surfaces_from_plugin_surfaces(
    surfaces: &[PluginSurface],
    allocations: &BTreeMap<bmux_plugin::layout::PluginLayoutId, ExtensionRect>,
    viewport: DamageRect,
) -> Vec<RetainedSurface> {
    surfaces
        .iter()
        .filter(|surface| surface.visible)
        .filter_map(|surface| {
            let rect = match &surface.target {
                PluginSurfaceTarget::Layout(id) => allocations.get(id).copied()?,
                PluginSurfaceTarget::Explicit(rect) => *rect,
            };
            let rect = DamageRect::new(rect.x, rect.y, rect.w, rect.h);
            if rect.w == 0 || rect.h == 0 {
                return None;
            }
            let local_to_absolute = |local: ExtensionRect| {
                DamageRect::new(
                    rect.x.saturating_add(local.x),
                    rect.y.saturating_add(local.y),
                    local.w,
                    local.h,
                )
            };
            let base_clip = intersect_rects(rect, viewport)?;
            let clip_rect = match surface.clip_rect.map(local_to_absolute) {
                Some(clip) => {
                    intersect_rects(clip, rect).and_then(|clip| intersect_rects(clip, viewport))?
                }
                None => base_clip,
            };
            let interactive_regions = if surface.accepts_input {
                surface
                    .interactive_regions
                    .iter()
                    .filter_map(|region| {
                        let rect = intersect_rects(local_to_absolute(region.rect), rect)
                            .and_then(|region| intersect_rects(region, clip_rect))?;
                        Some(RetainedInteractiveRegion {
                            id: Some(PluginSurfaceRegionId::new(&surface.id, &region.local_id)),
                            rect,
                            focusable: region.focusable,
                            cursor: region.cursor,
                            endpoint: region.endpoint.clone(),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let render_ops = surface
                .ops
                .iter()
                .map(|op| translate_surface_render_op(op, rect.x, rect.y))
                .collect();
            let mut builder = RetainedSurface::builder(surface.id.retained_id, rect)
                .layer(surface.layer)
                .z(surface.z)
                .modal(surface.modal)
                .revision(surface.revision)
                .render_ops(render_ops)
                .clip_rect(clip_rect)
                .retained_interactive_regions(interactive_regions);
            builder = if surface.opaque {
                builder.opaque()
            } else {
                builder.transparent()
            };
            Some(builder.build())
        })
        .collect()
}

fn translate_surface_render_op(op: &RenderOp, origin_x: u16, origin_y: u16) -> RenderOp {
    let translate_rect = |rect: ExtensionRect| {
        ExtensionRect::new(
            origin_x.saturating_add(rect.x),
            origin_y.saturating_add(rect.y),
            rect.w,
            rect.h,
        )
    };
    match op {
        RenderOp::TextRun { x, y, text, style } => RenderOp::TextRun {
            x: origin_x.saturating_add(*x),
            y: origin_y.saturating_add(*y),
            text: text.clone(),
            style: *style,
        },
        RenderOp::StyledText { x, y, spans } => RenderOp::StyledText {
            x: origin_x.saturating_add(*x),
            y: origin_y.saturating_add(*y),
            spans: spans.clone(),
        },
        RenderOp::ClearRect { rect, style } => RenderOp::ClearRect {
            rect: translate_rect(*rect),
            style: *style,
        },
        RenderOp::EraseRowSegment { x, y, width, style } => RenderOp::EraseRowSegment {
            x: origin_x.saturating_add(*x),
            y: origin_y.saturating_add(*y),
            width: *width,
            style: *style,
        },
        RenderOp::FillRect { rect, ch, style } => RenderOp::FillRect {
            rect: translate_rect(*rect),
            ch: *ch,
            style: *style,
        },
        RenderOp::Border {
            rect,
            glyphs,
            style,
        } => RenderOp::Border {
            rect: translate_rect(*rect),
            glyphs: *glyphs,
            style: *style,
        },
        RenderOp::CellGrid { x, y, rows } => RenderOp::CellGrid {
            x: origin_x.saturating_add(*x),
            y: origin_y.saturating_add(*y),
            rows: rows.clone(),
        },
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
    pub const fn modal(mut self, modal: bool) -> Self {
        self.surface.modal = modal;
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
    pub const fn revision(mut self, revision: u64) -> Self {
        self.surface.revision = Some(revision);
        self
    }

    #[must_use]
    pub const fn clip_rect(mut self, clip_rect: DamageRect) -> Self {
        self.surface.clip_rect = Some(clip_rect);
        self
    }

    #[must_use]
    pub fn interactive_region(mut self, region: DamageRect) -> Self {
        self.surface
            .interactive_regions
            .push(RetainedInteractiveRegion {
                id: None,
                rect: region,
                focusable: false,
                cursor: bmux_plugin::surface::PluginSurfaceCursor::Default,
                endpoint: None,
            });
        self
    }

    #[must_use]
    pub fn interactive_regions(mut self, regions: Vec<DamageRect>) -> Self {
        self.surface.interactive_regions = regions
            .into_iter()
            .map(|rect| RetainedInteractiveRegion {
                id: None,
                rect,
                focusable: false,
                cursor: bmux_plugin::surface::PluginSurfaceCursor::Default,
                endpoint: None,
            })
            .collect();
        self
    }

    #[must_use]
    pub fn retained_interactive_regions(mut self, regions: Vec<RetainedInteractiveRegion>) -> Self {
        self.surface.interactive_regions = regions;
        self
    }

    #[must_use]
    pub fn build(self) -> RetainedSurface {
        self.surface
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedSurfaceHit {
    pub surface_id: Uuid,
    pub surface_revision: Option<u64>,
    pub region_index: usize,
    pub region_id: Option<PluginSurfaceRegionId>,
    pub absolute_x: u16,
    pub absolute_y: u16,
    pub surface_x: u16,
    pub surface_y: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedPointerPhase {
    Enter,
    Move,
    Leave,
    Down,
    Up,
    Wheel,
    Drag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedPointerButton {
    Primary,
    Secondary,
    Middle,
    Other(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedPointerEvent {
    pub phase: RetainedPointerPhase,
    pub hit: RetainedSurfaceHit,
    pub button: Option<RetainedPointerButton>,
    pub wheel_delta: i16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedKeyboardEvent {
    pub target: PluginSurfaceRegionId,
    pub key: String,
    pub pressed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetainedInputEvent {
    Pointer(RetainedPointerEvent),
    Keyboard(RetainedKeyboardEvent),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedInputDispatch {
    pub endpoint: bmux_plugin::AttachInputEndpoint,
    pub event: RetainedInputEvent,
}

impl RetainedInputEvent {
    #[must_use]
    pub const fn target(&self) -> Option<&PluginSurfaceRegionId> {
        match self {
            Self::Pointer(event) => event.hit.region_id.as_ref(),
            Self::Keyboard(event) => Some(&event.target),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedInputQueue {
    maximum_events: usize,
    events: Vec<RetainedInputEvent>,
}

impl Default for RetainedInputQueue {
    fn default() -> Self {
        Self::new(1_024)
    }
}

impl RetainedInputQueue {
    #[must_use]
    pub const fn new(maximum_events: usize) -> Self {
        Self {
            maximum_events,
            events: Vec::new(),
        }
    }

    const fn has_capacity(&self) -> bool {
        self.events.len() < self.maximum_events
    }

    pub fn push_pointer(&mut self, event: RetainedPointerEvent) -> bool {
        if event.hit.region_id.is_none() || !self.has_capacity() {
            return false;
        }
        self.events.push(RetainedInputEvent::Pointer(event));
        true
    }

    pub fn push_key(
        &mut self,
        focus: &RetainedFocusState,
        key: impl Into<String>,
        pressed: bool,
    ) -> bool {
        if !self.has_capacity() {
            return false;
        }
        let Some(target) = focus.focused().cloned() else {
            return false;
        };
        self.events
            .push(RetainedInputEvent::Keyboard(RetainedKeyboardEvent {
                target,
                key: key.into(),
                pressed,
            }));
        true
    }

    #[must_use]
    pub fn drain_for_dispatch(
        &mut self,
        compositor: &RetainedCompositor,
    ) -> Vec<RetainedInputDispatch> {
        self.events
            .drain(..)
            .filter_map(|event| {
                let endpoint = compositor.endpoint_for_region(event.target()?)?.clone();
                Some(RetainedInputDispatch { endpoint, event })
            })
            .collect()
    }

    #[must_use]
    pub fn drain(&mut self) -> Vec<RetainedInputEvent> {
        self.events.drain(..).collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetainedPointerRouter {
    hovered: Option<RetainedSurfaceHit>,
    captured: Option<RetainedSurfaceHit>,
    preserve_capture_across_revisions: bool,
}

impl RetainedPointerRouter {
    #[must_use]
    pub const fn hovered(&self) -> Option<&RetainedSurfaceHit> {
        self.hovered.as_ref()
    }

    #[must_use]
    pub const fn captured(&self) -> Option<&RetainedSurfaceHit> {
        self.captured.as_ref()
    }

    pub fn capture(&mut self, hit: RetainedSurfaceHit) {
        self.captured = Some(hit);
        self.preserve_capture_across_revisions = false;
    }

    pub const fn preserve_capture_across_revisions(&mut self, preserve: bool) {
        self.preserve_capture_across_revisions = preserve;
    }

    pub const fn release_capture(&mut self) -> Option<RetainedSurfaceHit> {
        self.preserve_capture_across_revisions = false;
        self.captured.take()
    }

    #[must_use]
    pub fn route_terminal_mouse(
        &mut self,
        compositor: &RetainedCompositor,
        event: crossterm::event::MouseEvent,
    ) -> Vec<RetainedPointerEvent> {
        use crossterm::event::{MouseButton, MouseEventKind};

        let button = |button| match button {
            MouseButton::Left => RetainedPointerButton::Primary,
            MouseButton::Right => RetainedPointerButton::Secondary,
            MouseButton::Middle => RetainedPointerButton::Middle,
        };
        match event.kind {
            MouseEventKind::Moved => self.route_move(compositor, event.column, event.row),
            MouseEventKind::Down(next) => self
                .route_button(compositor, event.column, event.row, button(next), true)
                .into_iter()
                .collect(),
            MouseEventKind::Up(next) => self
                .route_button(compositor, event.column, event.row, button(next), false)
                .into_iter()
                .collect(),
            MouseEventKind::Drag(next) => self
                .route_drag(compositor, event.column, event.row, button(next))
                .into_iter()
                .collect(),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollRight => self
                .route_wheel(compositor, event.column, event.row, 1)
                .into_iter()
                .collect(),
            MouseEventKind::ScrollDown | MouseEventKind::ScrollLeft => self
                .route_wheel(compositor, event.column, event.row, -1)
                .into_iter()
                .collect(),
        }
    }

    #[must_use]
    pub fn route_move(
        &mut self,
        compositor: &RetainedCompositor,
        x: u16,
        y: u16,
    ) -> Vec<RetainedPointerEvent> {
        let next = compositor.hit_test(x, y);
        let same_target =
            self.hovered
                .as_ref()
                .zip(next.as_ref())
                .is_some_and(|(current, next)| {
                    current.surface_id == next.surface_id
                        && current.surface_revision == next.surface_revision
                        && current.region_index == next.region_index
                        && current.region_id == next.region_id
                });
        if same_target {
            self.hovered.clone_from(&next);
            return next
                .into_iter()
                .map(|hit| RetainedPointerEvent {
                    phase: RetainedPointerPhase::Move,
                    hit,
                    button: None,
                    wheel_delta: 0,
                })
                .collect();
        }

        let mut events = Vec::with_capacity(2);
        if let Some(hit) = self.hovered.take() {
            events.push(RetainedPointerEvent {
                phase: RetainedPointerPhase::Leave,
                hit,
                button: None,
                wheel_delta: 0,
            });
        }
        if let Some(hit) = next {
            events.push(RetainedPointerEvent {
                phase: RetainedPointerPhase::Enter,
                hit: hit.clone(),
                button: None,
                wheel_delta: 0,
            });
            self.hovered = Some(hit);
        }
        events
    }

    #[must_use]
    pub fn route_button(
        &self,
        compositor: &RetainedCompositor,
        x: u16,
        y: u16,
        button: RetainedPointerButton,
        pressed: bool,
    ) -> Option<RetainedPointerEvent> {
        compositor.hit_test(x, y).map(|hit| RetainedPointerEvent {
            phase: if pressed {
                RetainedPointerPhase::Down
            } else {
                RetainedPointerPhase::Up
            },
            hit,
            button: Some(button),
            wheel_delta: 0,
        })
    }

    #[must_use]
    pub fn route_wheel(
        &self,
        compositor: &RetainedCompositor,
        x: u16,
        y: u16,
        delta: i16,
    ) -> Option<RetainedPointerEvent> {
        compositor.hit_test(x, y).map(|hit| RetainedPointerEvent {
            phase: RetainedPointerPhase::Wheel,
            hit,
            button: None,
            wheel_delta: delta,
        })
    }

    #[must_use]
    pub fn route_drag(
        &self,
        compositor: &RetainedCompositor,
        x: u16,
        y: u16,
        button: RetainedPointerButton,
    ) -> Option<RetainedPointerEvent> {
        self.captured
            .as_ref()
            .and_then(|captured| {
                retarget_hit(
                    compositor,
                    captured,
                    x,
                    y,
                    self.preserve_capture_across_revisions,
                )
            })
            .or_else(|| compositor.hit_test(x, y))
            .map(|hit| RetainedPointerEvent {
                phase: RetainedPointerPhase::Drag,
                hit,
                button: Some(button),
                wheel_delta: 0,
            })
    }

    #[must_use]
    pub fn reconcile(&mut self, compositor: &RetainedCompositor) -> Vec<RetainedPointerEvent> {
        let mut events = Vec::new();
        if let Some(hovered) = self.hovered.as_ref()
            && !hit_target_is_present(compositor, hovered, false)
            && let Some(hit) = self.hovered.take()
        {
            events.push(RetainedPointerEvent {
                phase: RetainedPointerPhase::Leave,
                hit,
                button: None,
                wheel_delta: 0,
            });
        }
        if self.captured.as_ref().is_some_and(|captured| {
            !hit_target_is_present(compositor, captured, self.preserve_capture_across_revisions)
        }) {
            self.captured = None;
        }
        events
    }
}

fn hit_target_is_present(
    compositor: &RetainedCompositor,
    hit: &RetainedSurfaceHit,
    preserve_across_revisions: bool,
) -> bool {
    compositor.surfaces().values().any(|surface| {
        surface.id == hit.surface_id
            && (preserve_across_revisions || surface.revision == hit.surface_revision)
            && surface
                .interactive_regions
                .iter()
                .any(|region| region.id == hit.region_id)
    })
}

fn retarget_hit(
    compositor: &RetainedCompositor,
    captured: &RetainedSurfaceHit,
    x: u16,
    y: u16,
    preserve_across_revisions: bool,
) -> Option<RetainedSurfaceHit> {
    let surface = compositor.surfaces().get(&captured.surface_id)?;
    if !preserve_across_revisions && surface.revision != captured.surface_revision {
        return None;
    }
    let (region_index, _) = surface
        .interactive_regions
        .iter()
        .enumerate()
        .find(|(_, region)| region.id == captured.region_id)?;
    Some(RetainedSurfaceHit {
        surface_id: captured.surface_id,
        surface_revision: surface.revision,
        region_index,
        region_id: captured.region_id.clone(),
        absolute_x: x,
        absolute_y: y,
        surface_x: x.saturating_sub(surface.rect.x),
        surface_y: y.saturating_sub(surface.rect.y),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetainedFocusState {
    focused: Option<PluginSurfaceRegionId>,
}

impl RetainedFocusState {
    #[must_use]
    pub const fn focused(&self) -> Option<&PluginSurfaceRegionId> {
        self.focused.as_ref()
    }

    pub fn focus_hit(&mut self, compositor: &RetainedCompositor, hit: &RetainedSurfaceHit) -> bool {
        let Some(region) = compositor
            .surfaces()
            .get(&hit.surface_id)
            .and_then(|surface| surface.interactive_regions.get(hit.region_index))
        else {
            return false;
        };
        let Some(id) = region.id.as_ref().filter(|_| region.focusable) else {
            return false;
        };
        self.focused = Some(id.clone());
        true
    }

    pub const fn clear(&mut self) -> Option<PluginSurfaceRegionId> {
        self.focused.take()
    }

    pub fn reconcile(&mut self, compositor: &RetainedCompositor) -> bool {
        let Some(focused) = self.focused.as_ref() else {
            return false;
        };
        let present = compositor.surfaces().values().any(|surface| {
            surface
                .interactive_regions
                .iter()
                .any(|region| region.focusable && region.id.as_ref() == Some(focused))
        });
        if present {
            false
        } else {
            self.focused = None;
            true
        }
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
    pub fn hit_test(&self, x: u16, y: u16) -> Option<RetainedSurfaceHit> {
        let ordered = self.ordered_surfaces();
        let mut modal_floor = None;
        for (index, surface) in ordered.iter().enumerate() {
            if surface.modal && rect_contains_point(surface.paint_rect()?, x, y) {
                modal_floor = Some(index);
            }
        }
        let start = modal_floor.unwrap_or(0);
        ordered
            .into_iter()
            .enumerate()
            .rev()
            .take_while(|(index, _)| *index >= start)
            .find_map(|(_, surface)| {
                let paint_rect = surface.paint_rect()?;
                if !rect_contains_point(paint_rect, x, y) {
                    return None;
                }
                surface
                    .interactive_regions
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, region)| rect_contains_point(region.rect, x, y))
                    .map(|(region_index, region)| RetainedSurfaceHit {
                        surface_id: surface.id,
                        surface_revision: surface.revision,
                        region_index,
                        region_id: region.id.clone(),
                        absolute_x: x,
                        absolute_y: y,
                        surface_x: x.saturating_sub(surface.rect.x),
                        surface_y: y.saturating_sub(surface.rect.y),
                    })
            })
    }

    #[must_use]
    pub fn endpoint_for_region(
        &self,
        target: &PluginSurfaceRegionId,
    ) -> Option<&bmux_plugin::AttachInputEndpoint> {
        self.surfaces.values().find_map(|surface| {
            surface.interactive_regions.iter().find_map(|region| {
                (region.id.as_ref() == Some(target))
                    .then_some(region.endpoint.as_ref())
                    .flatten()
            })
        })
    }

    #[must_use]
    pub fn endpoint_for_hit(
        &self,
        hit: &RetainedSurfaceHit,
    ) -> Option<&bmux_plugin::AttachInputEndpoint> {
        let surface = self.surfaces.get(&hit.surface_id)?;
        if surface.revision != hit.surface_revision {
            return None;
        }
        let region = surface.interactive_regions.get(hit.region_index)?;
        if region.id != hit.region_id {
            return None;
        }
        region.endpoint.as_ref()
    }

    #[must_use]
    pub fn cursor_for_hit(
        &self,
        hit: &RetainedSurfaceHit,
    ) -> Option<bmux_plugin::surface::PluginSurfaceCursor> {
        let surface = self.surfaces.get(&hit.surface_id)?;
        if surface.revision != hit.surface_revision {
            return None;
        }
        let region = surface.interactive_regions.get(hit.region_index)?;
        if region.id != hit.region_id {
            return None;
        }
        Some(region.cursor)
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
                        modal: surface.modal,
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

#[must_use]
pub fn retained_damage_from_absolute_rects(
    rects: impl IntoIterator<Item = DamageRect>,
    viewport: DamageRect,
    policy: DamageCoalescingPolicy,
) -> RetainedDamage {
    coalesce_absolute_damage(rects.into_iter().collect(), viewport, policy)
}

#[must_use]
pub fn merge_retained_damages(
    damages: impl IntoIterator<Item = RetainedDamage>,
    viewport: DamageRect,
    policy: DamageCoalescingPolicy,
) -> RetainedDamage {
    let mut rects = Vec::new();
    for damage in damages {
        match damage {
            RetainedDamage::None => {}
            RetainedDamage::Full { .. } => return RetainedDamage::Full { viewport },
            RetainedDamage::Regions(next) => rects.extend(next),
        }
    }
    coalesce_absolute_damage(rects, viewport, policy)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedRepaintSurface {
    pub surface_id: Uuid,
    pub rect: DamageRect,
    pub layer: i16,
    pub z: i32,
    pub opaque: bool,
    pub modal: bool,
    pub opacity: RetainedOpacity,
    pub clip_rect: Option<DamageRect>,
    pub interactive_regions: Vec<RetainedInteractiveRegion>,
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
        for rect in frame_damage.vacated_surface_rects(surface.id) {
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
pub fn frame_damage_from_retained_repaint_plan(
    scene: &AttachScene,
    plan: &[RetainedRepaintSurface],
    policy: DamageCoalescingPolicy,
) -> FrameDamage {
    let mut frame_damage = FrameDamage::default();
    for repaint in plan {
        let Some(surface) = scene
            .surfaces
            .iter()
            .find(|surface| surface.id == repaint.surface_id && surface.visible)
        else {
            continue;
        };
        mark_retained_surface_repaint_damage(&mut frame_damage, surface, &repaint.damage, policy);
    }
    frame_damage
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

fn mark_retained_surface_repaint_damage(
    frame_damage: &mut FrameDamage,
    surface: &bmux_attach_layout_protocol::AttachSurface,
    absolute_damage: &[DamageRect],
    policy: DamageCoalescingPolicy,
) {
    let surface_rect = damage_rect_from_attach_rect(surface.rect);
    let content_rect = damage_rect_from_attach_rect(surface.content_rect);
    for rect in absolute_damage {
        if let Some(surface_damage) = intersect_rects(*rect, surface_rect) {
            frame_damage.mark_extension_surface_rect(
                surface.id,
                relative_damage_rect(surface_damage, surface_rect),
                (surface.rect.w, surface.rect.h),
                policy,
            );
        }
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if let Some(content_damage) = intersect_rects(*rect, content_rect) {
            frame_damage.mark_content_surface_rect(
                pane_id,
                relative_damage_rect(content_damage, content_rect),
                (surface.content_rect.w, surface.content_rect.h),
                policy,
            );
        }
    }
}

const fn relative_damage_rect(rect: DamageRect, origin: DamageRect) -> DamageRect {
    DamageRect::new(
        rect.x.saturating_sub(origin.x),
        rect.y.saturating_sub(origin.y),
        rect.w,
        rect.h,
    )
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

#[must_use]
pub const fn retained_layer_order(layer: AttachLayer) -> i16 {
    match layer {
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
            (Some(prev), Some(next)) if retained_surface_metadata_equal(prev, next) => {
                damaged.extend(render_ops_damage_between(prev, next));
            }
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

fn retained_surface_metadata_equal(previous: &RetainedSurface, next: &RetainedSurface) -> bool {
    previous.id == next.id
        && previous.rect == next.rect
        && previous.layer == next.layer
        && previous.z == next.z
        && previous.opaque == next.opaque
        && previous.opacity == next.opacity
        && previous.clip_rect == next.clip_rect
        && previous.interactive_regions == next.interactive_regions
}

fn render_ops_damage_between(
    previous: &RetainedSurface,
    next: &RetainedSurface,
) -> Vec<DamageRect> {
    let (
        RetainedSurfacePayload::RenderOps(previous_ops),
        RetainedSurfacePayload::RenderOps(next_ops),
    ) = (&previous.payload, &next.payload)
    else {
        return vec![previous.rect, next.rect];
    };
    let previous_rows = render_ops_row_signatures(previous_ops, previous.rect);
    let next_rows = render_ops_row_signatures(next_ops, next.rect);
    (previous.rect.y..previous.rect.bottom())
        .filter(|row| previous_rows.get(row) != next_rows.get(row))
        .filter_map(|row| {
            previous_rows
                .get(&row)
                .into_iter()
                .flatten()
                .chain(next_rows.get(&row).into_iter().flatten())
                .filter_map(|op| intersect_rects(render_op_damage_bounds(op), previous.rect))
                .map(|bounds| DamageRect::new(bounds.x, row, bounds.w, 1))
                .reduce(DamageRect::union)
        })
        .collect()
}

fn render_ops_row_signatures(
    ops: &[RenderOp],
    surface_rect: DamageRect,
) -> BTreeMap<u16, Vec<&RenderOp>> {
    let mut rows = BTreeMap::new();
    for op in ops {
        let bounds = render_op_damage_bounds(op);
        let Some(bounds) = intersect_rects(bounds, surface_rect) else {
            continue;
        };
        for row in bounds.y..bounds.bottom() {
            rows.entry(row).or_insert_with(Vec::new).push(op);
        }
    }
    rows
}

fn render_op_damage_bounds(op: &RenderOp) -> DamageRect {
    match op {
        RenderOp::TextRun { x, y, text, .. } => {
            DamageRect::new(*x, *y, render_text_width_u16(text), 1)
        }
        RenderOp::StyledText { x, y, spans } => DamageRect::new(
            *x,
            *y,
            spans.iter().fold(0_u16, |width, span| {
                width.saturating_add(render_text_width_u16(&span.text))
            }),
            1,
        ),
        RenderOp::ClearRect { rect, .. }
        | RenderOp::FillRect { rect, .. }
        | RenderOp::Border { rect, .. } => DamageRect::new(rect.x, rect.y, rect.w, rect.h),
        RenderOp::EraseRowSegment { x, y, width, .. } => DamageRect::new(*x, *y, *width, 1),
        RenderOp::CellGrid { x, y, rows } => DamageRect::new(
            *x,
            *y,
            rows.iter()
                .map(Vec::len)
                .max()
                .and_then(|width| u16::try_from(width).ok())
                .unwrap_or(u16::MAX),
            u16::try_from(rows.len()).unwrap_or(u16::MAX),
        ),
    }
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

const fn rect_contains_point(rect: DamageRect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.w)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.h)
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
    use bmux_plugin::surface::{
        PluginSurface, PluginSurfaceId, PluginSurfaceRegion, PluginSurfaceTarget,
    };
    use bmux_plugin::{RenderOp, RenderStyle};

    #[test]
    fn neutral_independent_split_surface_renders_through_retained_compositor() {
        use bmux_plugin::layout::{
            LayoutEdge, LayoutExtent, PluginLayoutRequest, resolve_plugin_layout,
        };

        let owner = "example.presentation";
        let layout_id = bmux_plugin::layout::PluginLayoutId::new(owner, "leading");
        let viewport = DamageRect::new(0, 0, 80, 24);
        let resolution = resolve_plugin_layout(
            ExtensionRect::new(viewport.x, viewport.y, viewport.w, viewport.h),
            (1, 1),
            &[PluginLayoutRequest::split(
                layout_id.clone(),
                0,
                LayoutEdge::Left,
                LayoutExtent::Cells(12),
            )],
        )
        .unwrap();
        let allocations = resolution
            .allocations
            .into_iter()
            .map(|allocation| (allocation.id, allocation.rect))
            .collect::<BTreeMap<_, _>>();
        let surface = PluginSurface::layout(
            PluginSurfaceId::new(owner, "surface", Uuid::from_u128(621)),
            1,
            layout_id,
            vec![RenderOp::text_run(0, 0, "independent", RenderStyle::new())],
        )
        .opaque(true);
        let lowered = retained_surfaces_from_plugin_surfaces(&[surface], &allocations, viewport);

        assert_eq!(lowered.len(), 1);
        assert_eq!(lowered[0].rect, DamageRect::new(0, 0, 12, 24));
        let mut compositor = RetainedCompositor::new();
        let damage =
            compositor.replace_surfaces(lowered, viewport, DamageCoalescingPolicy::default());
        let repaint = compositor.repaint_plan(&damage);
        assert_eq!(repaint.len(), 1);
        assert_eq!(repaint[0].surface_id, Uuid::from_u128(621));
        assert_eq!(repaint[0].damage, vec![DamageRect::new(0, 0, 12, 24)]);
        assert_eq!(resolution.remaining, ExtensionRect::new(12, 0, 68, 24));
    }

    #[test]
    fn plugin_surfaces_lower_through_layout_allocations_and_explicit_geometry() {
        let owner = "example.presentation";
        let layout_id = bmux_plugin::layout::PluginLayoutId::new(owner, "region");
        let layout_rect = ExtensionRect::new(4, 2, 20, 3);
        let allocations = BTreeMap::from([(layout_id.clone(), layout_rect)]);
        let surfaces = [
            PluginSurface {
                id: PluginSurfaceId::new(owner, "layout", Uuid::from_u128(501)),
                revision: 1,
                target: PluginSurfaceTarget::Layout(layout_id),
                clip_rect: None,
                interactive_regions: Vec::new(),
                accepts_input: false,
                layer: 3,
                z: 4,
                opaque: true,
                modal: false,
                visible: true,
                ops: vec![RenderOp::text_run(0, 0, "layout", RenderStyle::new())],
            },
            PluginSurface {
                id: PluginSurfaceId::new(owner, "overlay", Uuid::from_u128(502)),
                revision: 1,
                target: PluginSurfaceTarget::Explicit(ExtensionRect::new(30, 5, 10, 2)),
                clip_rect: None,
                interactive_regions: Vec::new(),
                accepts_input: false,
                layer: 5,
                z: 6,
                opaque: false,
                modal: false,
                visible: true,
                ops: vec![RenderOp::text_run(0, 0, "explicit", RenderStyle::new())],
            },
        ];

        let lowered = retained_surfaces_from_plugin_surfaces(
            &surfaces,
            &allocations,
            DamageRect::new(0, 0, 80, 24),
        );

        assert_eq!(lowered.len(), 2);
        assert_eq!(lowered[0].rect, DamageRect::new(4, 2, 20, 3));
        assert_eq!(
            lowered[0].payload,
            RetainedSurfacePayload::RenderOps(vec![RenderOp::text_run(
                4,
                2,
                "layout",
                RenderStyle::new(),
            )])
        );
        assert!(lowered[0].opaque);
        assert_eq!(lowered[1].rect, DamageRect::new(30, 5, 10, 2));
        assert_eq!(
            lowered[1].payload,
            RetainedSurfacePayload::RenderOps(vec![RenderOp::text_run(
                30,
                5,
                "explicit",
                RenderStyle::new(),
            )])
        );
        assert!(!lowered[1].opaque);
    }

    #[test]
    fn semantic_queue_resolves_typed_dispatch_and_drops_removed_targets() {
        let owner = "example.dispatch";
        let endpoint = bmux_plugin::AttachInputEndpoint {
            capability: "example.input".to_owned(),
            interface_id: "example-input".to_owned(),
            operation: "handle".to_owned(),
        };
        let mut compositor = RetainedCompositor::new();
        let surface = PluginSurface::layout(
            PluginSurfaceId::new(owner, "surface", Uuid::from_u128(620)),
            1,
            bmux_plugin::layout::PluginLayoutId::new(owner, "region"),
            Vec::new(),
        )
        .interactive_region(
            PluginSurfaceRegion::new("button", ExtensionRect::new(0, 0, 8, 4))
                .endpoint(endpoint.clone()),
        );
        let allocation = BTreeMap::from([(
            bmux_plugin::layout::PluginLayoutId::new(owner, "region"),
            ExtensionRect::new(2, 2, 8, 4),
        )]);
        compositor.replace_surfaces(
            retained_surfaces_from_plugin_surfaces(
                &[surface],
                &allocation,
                DamageRect::new(0, 0, 80, 24),
            ),
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let pointer = RetainedPointerRouter::default()
            .route_button(&compositor, 3, 3, RetainedPointerButton::Primary, true)
            .unwrap();
        let mut queue = RetainedInputQueue::default();
        assert!(queue.push_pointer(pointer.clone()));
        let dispatch = queue.drain_for_dispatch(&compositor);
        assert_eq!(dispatch.len(), 1);
        assert_eq!(dispatch[0].endpoint, endpoint);

        assert!(queue.push_pointer(pointer));
        compositor.replace_surfaces(
            [],
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        assert!(queue.drain_for_dispatch(&compositor).is_empty());
    }

    #[test]
    fn committed_hit_resolves_typed_endpoint_and_rejects_stale_revision() {
        let owner = "example.endpoint";
        let endpoint = bmux_plugin::AttachInputEndpoint {
            capability: "example.input".to_owned(),
            interface_id: "example-input".to_owned(),
            operation: "handle".to_owned(),
        };
        let mut compositor = RetainedCompositor::new();
        let surface = PluginSurface::layout(
            PluginSurfaceId::new(owner, "surface", Uuid::from_u128(619)),
            1,
            bmux_plugin::layout::PluginLayoutId::new(owner, "region"),
            Vec::new(),
        )
        .interactive_region(
            PluginSurfaceRegion::new("button", ExtensionRect::new(0, 0, 8, 4))
                .endpoint(endpoint.clone()),
        );
        let allocations = BTreeMap::from([(
            bmux_plugin::layout::PluginLayoutId::new(owner, "region"),
            ExtensionRect::new(2, 2, 8, 4),
        )]);
        let lowered = retained_surfaces_from_plugin_surfaces(
            &[surface],
            &allocations,
            DamageRect::new(0, 0, 80, 24),
        );
        compositor.replace_surfaces(
            lowered,
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let mut hit = compositor.hit_test(3, 3).unwrap();
        assert_eq!(compositor.endpoint_for_hit(&hit), Some(&endpoint));
        hit.surface_revision = Some(2);
        assert_eq!(compositor.endpoint_for_hit(&hit), None);
    }

    #[test]
    fn committed_hit_resolves_cursor_role_and_rejects_stale_revision() {
        let owner = "example.cursor";
        let mut compositor = RetainedCompositor::new();
        let surface = PluginSurface {
            id: PluginSurfaceId::new(owner, "surface", Uuid::from_u128(618)),
            revision: 1,
            target: PluginSurfaceTarget::Explicit(ExtensionRect::new(2, 2, 8, 4)),
            clip_rect: None,
            interactive_regions: vec![PluginSurfaceRegion {
                local_id: "link".to_owned(),
                rect: ExtensionRect::new(0, 0, 8, 4),
                focusable: false,
                cursor: bmux_plugin::surface::PluginSurfaceCursor::Pointer,
                endpoint: None,
            }],
            accepts_input: true,
            layer: 0,
            z: 0,
            opaque: false,
            modal: false,
            visible: true,
            ops: Vec::new(),
        };
        let lowered = retained_surfaces_from_plugin_surfaces(
            &[surface],
            &BTreeMap::new(),
            DamageRect::new(0, 0, 80, 24),
        );
        compositor.replace_surfaces(
            lowered,
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let mut hit = compositor.hit_test(3, 3).unwrap();
        assert_eq!(
            compositor.cursor_for_hit(&hit),
            Some(bmux_plugin::surface::PluginSurfaceCursor::Pointer)
        );
        hit.surface_revision = Some(2);
        assert_eq!(compositor.cursor_for_hit(&hit), None);
    }

    #[test]
    fn retained_input_queue_is_bounded_without_evicting_committed_events() {
        let target = PluginSurfaceRegionId {
            owner_plugin_id: "owner".to_owned(),
            surface_local_id: "surface".to_owned(),
            region_local_id: "region".to_owned(),
        };
        let focus = RetainedFocusState {
            focused: Some(target),
        };
        let mut queue = RetainedInputQueue::new(1);
        assert!(queue.push_key(&focus, "first", true));
        assert!(!queue.push_key(&focus, "second", true));
        let drained = queue.drain();
        assert!(matches!(
            drained.as_slice(),
            [RetainedInputEvent::Keyboard(RetainedKeyboardEvent { key, .. })] if key == "first"
        ));
    }

    #[test]
    fn retained_input_queue_accepts_semantic_pointer_and_focused_keyboard_events() {
        let owner = "example.queue";
        let mut compositor = RetainedCompositor::new();
        let surface = PluginSurface {
            id: PluginSurfaceId::new(owner, "surface", Uuid::from_u128(617)),
            revision: 1,
            target: PluginSurfaceTarget::Explicit(ExtensionRect::new(2, 2, 8, 4)),
            clip_rect: None,
            interactive_regions: vec![PluginSurfaceRegion {
                local_id: "field".to_owned(),
                rect: ExtensionRect::new(0, 0, 8, 4),
                focusable: true,
                cursor: bmux_plugin::surface::PluginSurfaceCursor::Text,
                endpoint: None,
            }],
            accepts_input: true,
            layer: 0,
            z: 0,
            opaque: false,
            modal: false,
            visible: true,
            ops: Vec::new(),
        };
        let lowered = retained_surfaces_from_plugin_surfaces(
            &[surface],
            &BTreeMap::new(),
            DamageRect::new(0, 0, 80, 24),
        );
        compositor.replace_surfaces(
            lowered,
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let hit = compositor.hit_test(3, 3).unwrap();
        let mut focus = RetainedFocusState::default();
        assert!(focus.focus_hit(&compositor, &hit));
        let pointer = RetainedPointerRouter::default()
            .route_button(&compositor, 3, 3, RetainedPointerButton::Primary, true)
            .unwrap();
        let mut queue = RetainedInputQueue::default();
        assert!(queue.push_pointer(pointer));
        assert!(queue.push_key(&focus, "enter", true));

        assert!(matches!(queue.drain().as_slice(), [
            RetainedInputEvent::Pointer(_),
            RetainedInputEvent::Keyboard(RetainedKeyboardEvent { key, pressed: true, .. })
        ] if key == "enter"));
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn stale_hit_and_capture_cannot_cross_surface_revision() {
        let mut compositor = RetainedCompositor::new();
        let viewport = DamageRect::new(0, 0, 80, 24);
        let make_surface = |revision| {
            RetainedSurface::builder(Uuid::from_u128(616), DamageRect::new(2, 2, 8, 4))
                .revision(revision)
                .interactive_region(DamageRect::new(2, 2, 8, 4))
                .build()
        };
        compositor.replace_surfaces(
            [make_surface(1)],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        let stale = compositor.hit_test(3, 3).unwrap();
        let mut router = RetainedPointerRouter::default();
        router.capture(stale.clone());

        compositor.replace_surfaces(
            [make_surface(2)],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        assert!(
            router
                .route_drag(&compositor, 20, 10, RetainedPointerButton::Primary)
                .is_none()
        );
        let _ = router.reconcile(&compositor);
        assert!(router.captured().is_none());
        assert_ne!(
            compositor.hit_test(3, 3).unwrap().surface_revision,
            stale.surface_revision
        );
    }

    #[test]
    fn retained_focus_accepts_only_focusable_semantic_regions_and_cleans_up() {
        let owner = "example.focus";
        let mut compositor = RetainedCompositor::new();
        let surface = PluginSurface {
            id: PluginSurfaceId::new(owner, "surface", Uuid::from_u128(615)),
            revision: 1,
            target: PluginSurfaceTarget::Explicit(ExtensionRect::new(2, 2, 8, 4)),
            clip_rect: None,
            interactive_regions: vec![PluginSurfaceRegion {
                local_id: "field".to_owned(),
                rect: ExtensionRect::new(0, 0, 8, 4),
                focusable: true,
                cursor: bmux_plugin::surface::PluginSurfaceCursor::Text,
                endpoint: None,
            }],
            accepts_input: true,
            layer: 0,
            z: 0,
            opaque: false,
            modal: false,
            visible: true,
            ops: Vec::new(),
        };
        let lowered = retained_surfaces_from_plugin_surfaces(
            &[surface],
            &BTreeMap::new(),
            DamageRect::new(0, 0, 80, 24),
        );
        compositor.replace_surfaces(
            lowered,
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let hit = compositor.hit_test(3, 3).unwrap();
        let mut focus = RetainedFocusState::default();
        assert!(focus.focus_hit(&compositor, &hit));
        assert_eq!(focus.focused().unwrap().region_local_id, "field");
        assert_eq!(
            compositor.surfaces()[&Uuid::from_u128(615)].interactive_regions[0].cursor,
            bmux_plugin::surface::PluginSurfaceCursor::Text
        );

        compositor.replace_surfaces(
            [],
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        assert!(focus.reconcile(&compositor));
        assert!(focus.focused().is_none());
    }

    #[test]
    fn pointer_capture_continues_drag_routing_and_cleans_up_removed_target() {
        let mut compositor = RetainedCompositor::new();
        let viewport = DamageRect::new(0, 0, 80, 24);
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(614), DamageRect::new(2, 2, 8, 4))
                    .interactive_region(DamageRect::new(2, 2, 8, 4))
                    .build(),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        let mut router = RetainedPointerRouter::default();
        let hit = compositor.hit_test(4, 3).unwrap();
        router.capture(hit);

        let dragged = router
            .route_drag(&compositor, 40, 20, RetainedPointerButton::Primary)
            .unwrap();
        assert_eq!(dragged.hit.surface_id, Uuid::from_u128(614));
        assert_eq!(dragged.hit.absolute_x, 40);
        assert_eq!(dragged.hit.surface_x, 38);
        assert!(router.release_capture().is_some());
        assert!(
            router
                .route_drag(&compositor, 40, 20, RetainedPointerButton::Primary)
                .is_none()
        );

        router.capture(compositor.hit_test(4, 3).unwrap());
        router.preserve_capture_across_revisions(true);
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(614), DamageRect::new(2, 2, 8, 4))
                    .revision(2)
                    .interactive_region(DamageRect::new(2, 2, 8, 4))
                    .build(),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        let dragged = router
            .route_drag(&compositor, 30, 10, RetainedPointerButton::Primary)
            .expect("stable region capture survives surface revision");
        assert_eq!(dragged.hit.surface_revision, Some(2));

        router.capture(compositor.hit_test(4, 3).unwrap());
        compositor.replace_surfaces([], viewport, DamageCoalescingPolicy::default());
        assert!(router.reconcile(&compositor).is_empty());
        assert!(router.captured().is_none());
    }

    #[test]
    fn terminal_mouse_normalization_routes_into_semantic_pointer_phases() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(624), DamageRect::new(2, 2, 8, 4))
                    .interactive_region(DamageRect::new(2, 2, 8, 4))
                    .build(),
            ],
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let mut router = RetainedPointerRouter::default();
        let event = |kind| MouseEvent {
            kind,
            column: 3,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            router.route_terminal_mouse(&compositor, event(MouseEventKind::Moved))[0].phase,
            RetainedPointerPhase::Enter
        );
        assert_eq!(
            router
                .route_terminal_mouse(&compositor, event(MouseEventKind::Down(MouseButton::Left)))
                [0]
            .phase,
            RetainedPointerPhase::Down
        );
        assert_eq!(
            router.route_terminal_mouse(&compositor, event(MouseEventKind::ScrollDown))[0]
                .wheel_delta,
            -1
        );
    }

    #[test]
    fn generic_surface_routes_complete_pointer_workflow_and_cleanup() {
        let owner = "example.interactive";
        let endpoint = bmux_plugin::AttachInputEndpoint {
            capability: "example.input".to_owned(),
            interface_id: "example-input".to_owned(),
            operation: "handle".to_owned(),
        };
        let surface = PluginSurface::layout(
            PluginSurfaceId::new(owner, "surface", Uuid::from_u128(625)),
            1,
            bmux_plugin::layout::PluginLayoutId::new(owner, "region"),
            Vec::new(),
        )
        .interactive_region(
            PluginSurfaceRegion::new("item", ExtensionRect::new(0, 0, 12, 6))
                .endpoint(endpoint.clone()),
        );
        let allocations = BTreeMap::from([(
            bmux_plugin::layout::PluginLayoutId::new(owner, "region"),
            ExtensionRect::new(2, 2, 12, 6),
        )]);
        let viewport = DamageRect::new(0, 0, 80, 24);
        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces(
            retained_surfaces_from_plugin_surfaces(&[surface], &allocations, viewport),
            viewport,
            DamageCoalescingPolicy::default(),
        );
        let mut router = RetainedPointerRouter::default();

        let hover = router.route_move(&compositor, 4, 3);
        assert_eq!(hover.len(), 1);
        assert_eq!(hover[0].phase, RetainedPointerPhase::Enter);
        assert_eq!((hover[0].hit.surface_x, hover[0].hit.surface_y), (2, 1));

        let click = router
            .route_button(&compositor, 4, 3, RetainedPointerButton::Primary, true)
            .expect("click");
        assert_eq!(click.phase, RetainedPointerPhase::Down);
        router.capture(click.hit.clone());

        let wheel = router.route_wheel(&compositor, 4, 3, -1).expect("wheel");
        assert_eq!(wheel.phase, RetainedPointerPhase::Wheel);
        assert_eq!(wheel.wheel_delta, -1);

        let drag = router
            .route_drag(&compositor, 40, 20, RetainedPointerButton::Primary)
            .expect("captured drag");
        assert_eq!(drag.phase, RetainedPointerPhase::Drag);
        assert_eq!((drag.hit.absolute_x, drag.hit.absolute_y), (40, 20));

        let mut queue = RetainedInputQueue::default();
        assert!(queue.push_pointer(click));
        assert!(queue.push_pointer(wheel));
        assert!(queue.push_pointer(drag));
        assert!(
            queue
                .drain_for_dispatch(&compositor)
                .into_iter()
                .all(|dispatch| dispatch.endpoint == endpoint)
        );

        compositor.replace_surfaces([], viewport, DamageCoalescingPolicy::default());
        let cleanup = router.reconcile(&compositor);
        assert_eq!(cleanup.len(), 1);
        assert_eq!(cleanup[0].phase, RetainedPointerPhase::Leave);
        assert!(router.hovered().is_none());
        assert!(router.captured().is_none());
    }

    #[test]
    fn modal_surface_blocks_input_fallthrough_inside_its_scope() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(622), DamageRect::new(0, 0, 20, 10))
                    .layer(0)
                    .interactive_region(DamageRect::new(0, 0, 20, 10))
                    .build(),
                RetainedSurface::builder(Uuid::from_u128(623), DamageRect::new(2, 2, 10, 5))
                    .layer(10)
                    .modal(true)
                    .build(),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(compositor.hit_test(3, 3), None);
        assert_eq!(
            compositor.hit_test(15, 3).unwrap().surface_id,
            Uuid::from_u128(622)
        );
    }

    #[test]
    #[ignore = "manual performance baseline; run with --release --ignored --nocapture"]
    fn pointer_move_routing_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const ITEM_COUNT: usize = 200;
        const ITERATIONS: u32 = 100_000;
        let viewport = DamageRect::new(0, 0, 80, 240);
        let retained = (0..ITEM_COUNT)
            .map(|index| {
                let y = u16::try_from(index).unwrap();
                RetainedSurface::builder(
                    Uuid::from_u128(u128::try_from(index + 1).unwrap()),
                    DamageRect::new(0, y, 40, 1),
                )
                .interactive_region(DamageRect::new(0, y, 40, 1))
                .build()
            })
            .collect::<Vec<_>>();
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(retained, viewport, DamageCoalescingPolicy::default());
        let mut router = RetainedPointerRouter::default();
        let started = Instant::now();
        for iteration in 0..ITERATIONS {
            let row = u16::try_from(iteration as usize % ITEM_COUNT).unwrap();
            black_box(router.route_move(&compositor, 4, row));
        }
        let average_ns = started.elapsed().as_nanos() / u128::from(ITERATIONS);
        println!(
            "pointer regions={ITEM_COUNT} iterations={ITERATIONS} move_average_ns={average_ns}"
        );
    }

    #[test]
    fn pointer_router_routes_button_wheel_and_drag_phases() {
        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(613), DamageRect::new(2, 2, 8, 4))
                    .interactive_region(DamageRect::new(2, 2, 8, 4))
                    .build(),
            ],
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let router = RetainedPointerRouter::default();

        let down = router
            .route_button(&compositor, 4, 3, RetainedPointerButton::Primary, true)
            .unwrap();
        assert_eq!(down.phase, RetainedPointerPhase::Down);
        assert_eq!(down.button, Some(RetainedPointerButton::Primary));
        assert_eq!(
            router
                .route_button(&compositor, 4, 3, RetainedPointerButton::Primary, false)
                .unwrap()
                .phase,
            RetainedPointerPhase::Up
        );
        let wheel = router.route_wheel(&compositor, 4, 3, -3).unwrap();
        assert_eq!(wheel.phase, RetainedPointerPhase::Wheel);
        assert_eq!(wheel.wheel_delta, -3);
        assert_eq!(
            router
                .route_drag(&compositor, 4, 3, RetainedPointerButton::Primary)
                .unwrap()
                .phase,
            RetainedPointerPhase::Drag
        );
        assert!(router.route_wheel(&compositor, 40, 20, 1).is_none());
    }

    #[test]
    fn pointer_router_replaces_hover_across_surface_revision() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let surface = |revision| {
            RetainedSurface::builder(Uuid::from_u128(625), DamageRect::new(2, 2, 8, 4))
                .revision(revision)
                .interactive_region(DamageRect::new(2, 2, 8, 4))
                .build()
        };
        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces([surface(1)], viewport, DamageCoalescingPolicy::default());
        let mut router = RetainedPointerRouter::default();
        assert_eq!(
            router.route_move(&compositor, 3, 3)[0].phase,
            RetainedPointerPhase::Enter
        );

        compositor.replace_surfaces([surface(2)], viewport, DamageCoalescingPolicy::default());
        let events = router.route_move(&compositor, 3, 3);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].phase, RetainedPointerPhase::Leave);
        assert_eq!(events[0].hit.surface_revision, Some(1));
        assert_eq!(events[1].phase, RetainedPointerPhase::Enter);
        assert_eq!(events[1].hit.surface_revision, Some(2));
    }

    #[test]
    fn pointer_router_emits_enter_move_leave_and_cleans_up_removed_target() {
        let mut compositor = RetainedCompositor::new();
        let viewport = DamageRect::new(0, 0, 80, 24);
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(610), DamageRect::new(5, 3, 10, 5))
                    .interactive_region(DamageRect::new(5, 3, 10, 5))
                    .build(),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        let mut router = RetainedPointerRouter::default();

        let entered = router.route_move(&compositor, 7, 4);
        assert_eq!(entered.len(), 1);
        assert_eq!(entered[0].phase, RetainedPointerPhase::Enter);
        let moved = router.route_move(&compositor, 8, 4);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].phase, RetainedPointerPhase::Move);
        let left = router.route_move(&compositor, 30, 20);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].phase, RetainedPointerPhase::Leave);

        let _ = router.route_move(&compositor, 7, 4);
        compositor.replace_surfaces([], viewport, DamageCoalescingPolicy::default());
        assert_eq!(
            router.reconcile(&compositor)[0].phase,
            RetainedPointerPhase::Leave
        );
        assert!(router.hovered().is_none());
    }

    #[test]
    fn pointer_router_orders_leave_before_enter_between_targets() {
        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(611), DamageRect::new(0, 0, 4, 2))
                    .interactive_region(DamageRect::new(0, 0, 4, 2))
                    .build(),
                RetainedSurface::builder(Uuid::from_u128(612), DamageRect::new(5, 0, 4, 2))
                    .interactive_region(DamageRect::new(5, 0, 4, 2))
                    .build(),
            ],
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        let mut router = RetainedPointerRouter::default();
        let _ = router.route_move(&compositor, 1, 1);
        let events = router.route_move(&compositor, 6, 1);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].phase, RetainedPointerPhase::Leave);
        assert_eq!(events[1].phase, RetainedPointerPhase::Enter);
    }

    #[test]
    fn retained_hit_test_prefers_topmost_region_and_returns_local_coordinates() {
        let mut compositor = RetainedCompositor::new();
        let viewport = DamageRect::new(0, 0, 80, 24);
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(600), DamageRect::new(5, 3, 10, 5))
                    .layer(1)
                    .z(0)
                    .interactive_region(DamageRect::new(5, 3, 10, 5))
                    .build(),
                RetainedSurface::builder(Uuid::from_u128(601), DamageRect::new(8, 4, 10, 5))
                    .layer(2)
                    .z(0)
                    .interactive_regions(vec![
                        DamageRect::new(8, 4, 6, 3),
                        DamageRect::new(10, 5, 2, 1),
                    ])
                    .build(),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(
            compositor.hit_test(10, 5),
            Some(RetainedSurfaceHit {
                surface_id: Uuid::from_u128(601),
                surface_revision: None,
                region_index: 1,
                region_id: None,
                absolute_x: 10,
                absolute_y: 5,
                surface_x: 2,
                surface_y: 1,
            })
        );
        assert_eq!(
            compositor.hit_test(6, 4).unwrap().surface_id,
            Uuid::from_u128(600)
        );
        assert_eq!(compositor.hit_test(30, 20), None);
    }

    #[test]
    fn retained_hit_test_respects_surface_clip() {
        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(602), DamageRect::new(5, 3, 10, 5))
                    .clip_rect(DamageRect::new(7, 4, 2, 2))
                    .interactive_region(DamageRect::new(5, 3, 10, 5))
                    .build(),
            ],
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );

        assert!(compositor.hit_test(7, 4).is_some());
        assert_eq!(compositor.hit_test(6, 4), None);
    }

    #[test]
    fn plugin_surface_lowering_clips_and_gates_local_input_regions() {
        let owner = "example.presentation";
        let surface = PluginSurface {
            id: PluginSurfaceId::new(owner, "interactive", Uuid::from_u128(506)),
            revision: 1,
            target: PluginSurfaceTarget::Explicit(ExtensionRect::new(10, 4, 8, 4)),
            clip_rect: Some(ExtensionRect::new(2, 1, 4, 2)),
            interactive_regions: vec![
                bmux_plugin::surface::PluginSurfaceRegion {
                    local_id: "whole".to_owned(),
                    rect: ExtensionRect::new(0, 0, 8, 4),
                    focusable: false,
                    cursor: bmux_plugin::surface::PluginSurfaceCursor::Default,
                    endpoint: None,
                },
                bmux_plugin::surface::PluginSurfaceRegion {
                    local_id: "inner".to_owned(),
                    rect: ExtensionRect::new(3, 1, 2, 1),
                    focusable: false,
                    cursor: bmux_plugin::surface::PluginSurfaceCursor::Default,
                    endpoint: None,
                },
            ],
            accepts_input: true,
            layer: 0,
            z: 0,
            opaque: false,
            modal: false,
            visible: true,
            ops: Vec::new(),
        };

        let lowered = retained_surfaces_from_plugin_surfaces(
            &[surface],
            &BTreeMap::new(),
            DamageRect::new(0, 0, 80, 24),
        );

        assert_eq!(lowered[0].clip_rect, Some(DamageRect::new(12, 5, 4, 2)));
        assert_eq!(
            lowered[0].interactive_regions,
            vec![DamageRect::new(12, 5, 4, 2), DamageRect::new(13, 5, 2, 1)]
        );
        assert_eq!(
            lowered[0].interactive_regions[1].id,
            Some(PluginSurfaceRegionId {
                owner_plugin_id: owner.to_owned(),
                surface_local_id: "interactive".to_owned(),
                region_local_id: "inner".to_owned(),
            })
        );
        let mut compositor = RetainedCompositor::new();
        compositor.replace_surfaces(
            lowered,
            DamageRect::new(0, 0, 80, 24),
            DamageCoalescingPolicy::default(),
        );
        assert_eq!(
            compositor.hit_test(13, 5).unwrap().region_id,
            Some(PluginSurfaceRegionId {
                owner_plugin_id: owner.to_owned(),
                surface_local_id: "interactive".to_owned(),
                region_local_id: "inner".to_owned(),
            })
        );
    }

    #[test]
    fn plugin_surface_lowering_skips_hidden_unresolved_and_zero_area_surfaces() {
        let owner = "example.presentation";
        let surfaces = [
            PluginSurface {
                id: PluginSurfaceId::new(owner, "hidden", Uuid::from_u128(503)),
                revision: 1,
                target: PluginSurfaceTarget::Explicit(ExtensionRect::new(1, 1, 4, 1)),
                clip_rect: None,
                interactive_regions: Vec::new(),
                accepts_input: false,
                layer: 0,
                z: 0,
                opaque: true,
                modal: false,
                visible: false,
                ops: Vec::new(),
            },
            PluginSurface {
                id: PluginSurfaceId::new(owner, "missing", Uuid::from_u128(504)),
                revision: 1,
                target: PluginSurfaceTarget::Layout(bmux_plugin::layout::PluginLayoutId::new(
                    owner, "missing",
                )),
                clip_rect: None,
                interactive_regions: Vec::new(),
                accepts_input: false,
                layer: 0,
                z: 0,
                opaque: true,
                modal: false,
                visible: true,
                ops: Vec::new(),
            },
            PluginSurface {
                id: PluginSurfaceId::new(owner, "empty", Uuid::from_u128(505)),
                revision: 1,
                target: PluginSurfaceTarget::Explicit(ExtensionRect::new(1, 1, 0, 1)),
                clip_rect: None,
                interactive_regions: Vec::new(),
                accepts_input: false,
                layer: 0,
                z: 0,
                opaque: true,
                modal: false,
                visible: true,
                ops: Vec::new(),
            },
        ];

        assert!(
            retained_surfaces_from_plugin_surfaces(
                &surfaces,
                &BTreeMap::new(),
                DamageRect::new(0, 0, 80, 24),
            )
            .is_empty()
        );
    }

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
    #[ignore = "manual performance baseline; run with --release --ignored --nocapture"]
    fn plugin_surface_reconciliation_benchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        const ITEM_COUNT: usize = 200;
        const ITERATIONS: u32 = 10_000;
        let viewport = DamageRect::new(0, 0, 240, 200);
        let build = |changed: Option<usize>| {
            (0..ITEM_COUNT)
                .map(|index| {
                    let y = u16::try_from(index).unwrap();
                    let text = if changed == Some(index) {
                        format!("changed-{index}")
                    } else {
                        format!("item-{index}")
                    };
                    RetainedSurface::builder(
                        Uuid::from_u128(u128::try_from(index + 1).unwrap()),
                        DamageRect::new(0, y, 40, 1),
                    )
                    .revision(1)
                    .render_ops(vec![RenderOp::text_run(0, y, text, RenderStyle::default())])
                    .build()
                })
                .collect::<Vec<_>>()
        };
        let stable = build(None);
        let changed = build(Some(ITEM_COUNT / 2));
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            stable.clone(),
            viewport,
            DamageCoalescingPolicy::default(),
        );

        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(compositor.replace_surfaces(
                stable.clone(),
                viewport,
                DamageCoalescingPolicy::default(),
            ));
        }
        let no_op_ns = started.elapsed().as_nanos() / u128::from(ITERATIONS);

        let row_changed = (0..ITEM_COUNT)
            .map(|index| {
                let y = u16::try_from(index).unwrap();
                let text = if (80..120).contains(&index) {
                    format!("row-changed-{index}")
                } else {
                    format!("item-{index}")
                };
                RetainedSurface::builder(
                    Uuid::from_u128(u128::try_from(index + 1).unwrap()),
                    DamageRect::new(0, y, 40, 1),
                )
                .revision(1)
                .render_ops(vec![RenderOp::text_run(0, y, text, RenderStyle::default())])
                .build()
            })
            .collect::<Vec<_>>();
        let full_changed = (0..ITEM_COUNT)
            .map(|index| {
                let y = u16::try_from(index).unwrap();
                RetainedSurface::builder(
                    Uuid::from_u128(u128::try_from(index + 1).unwrap()),
                    DamageRect::new(0, y, 40, 1),
                )
                .revision(1)
                .render_ops(vec![RenderOp::text_run(
                    0,
                    y,
                    format!("full-changed-{index}"),
                    RenderStyle::default(),
                )])
                .build()
            })
            .collect::<Vec<_>>();
        let mut measure_alternating = |alternate: &[RetainedSurface]| {
            let started = Instant::now();
            for iteration in 0..ITERATIONS {
                let next = if iteration % 2 == 0 {
                    alternate.to_vec()
                } else {
                    stable.clone()
                };
                black_box(compositor.replace_surfaces(
                    next,
                    viewport,
                    DamageCoalescingPolicy::default(),
                ));
            }
            started.elapsed().as_nanos() / u128::from(ITERATIONS)
        };
        let incremental_ns = measure_alternating(&changed);
        let row_ns = measure_alternating(&row_changed);
        let full_ns = measure_alternating(&full_changed);
        println!(
            "plugin surfaces={ITEM_COUNT} iterations={ITERATIONS} no_op_average_ns={no_op_ns} one_item_average_ns={incremental_ns} one_row_average_ns={row_ns} full_surface_average_ns={full_ns}"
        );
    }

    #[test]
    fn plugin_surface_lowering_preserves_every_supported_retained_paint_item() {
        use bmux_plugin::{BorderGlyphs, RenderCell, RenderTextSpan};

        let owner = "example.paint-items";
        let surface_id = PluginSurfaceId::new(owner, "main", Uuid::from_u128(506));
        let style = RenderStyle::new().bold().underline();
        let ops = vec![
            RenderOp::text_run(1, 1, "text", style),
            RenderOp::styled_text(
                1,
                2,
                vec![
                    RenderTextSpan::new("first", style),
                    RenderTextSpan::new("second", RenderStyle::new()),
                ],
            ),
            RenderOp::clear_rect(ExtensionRect::new(1, 3, 4, 1), style),
            RenderOp::erase_row_segment(1, 4, 4, style),
            RenderOp::fill_rect(ExtensionRect::new(1, 5, 4, 1), 'x', style),
            RenderOp::border(
                ExtensionRect::new(0, 0, 10, 8),
                BorderGlyphs::rounded(),
                style,
            ),
            RenderOp::cell_grid(
                1,
                6,
                vec![vec![
                    RenderCell::new('a', style),
                    RenderCell::sparse(RenderStyle::new()),
                ]],
            ),
        ];
        let surface = PluginSurface {
            id: surface_id,
            revision: 7,
            target: PluginSurfaceTarget::Explicit(ExtensionRect::new(5, 3, 10, 8)),
            clip_rect: Some(ExtensionRect::new(1, 1, 8, 6)),
            interactive_regions: Vec::new(),
            accepts_input: false,
            layer: 0,
            z: 0,
            opaque: true,
            modal: false,
            visible: true,
            ops: ops.clone(),
        };

        let lowered = retained_surfaces_from_plugin_surfaces(
            &[surface],
            &BTreeMap::new(),
            DamageRect::new(0, 0, 80, 24),
        );

        assert_eq!(lowered.len(), 1);
        assert_eq!(lowered[0].revision, Some(7));
        assert_eq!(lowered[0].opacity, RetainedOpacity::Opaque);
        assert_eq!(lowered[0].clip_rect, Some(DamageRect::new(6, 4, 8, 6)));
        let expected_ops = ops
            .iter()
            .map(|op| translate_surface_render_op(op, 5, 3))
            .collect();
        assert_eq!(
            lowered[0].payload,
            RetainedSurfacePayload::RenderOps(expected_ops)
        );
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
    fn replace_surfaces_limits_render_op_changes_to_changed_rows() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let rect = DamageRect::new(10, 5, 12, 4);
        let rows = |middle: &str| {
            vec![
                RenderOp::text_run(10, 5, "top", RenderStyle::default()),
                RenderOp::text_run(10, 6, middle, RenderStyle::default()),
                RenderOp::text_run(10, 7, "bottom", RenderStyle::default()),
                RenderOp::text_run(10, 8, "footer", RenderStyle::default()),
            ]
        };
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            [RetainedSurface::builder(Uuid::from_u128(1), rect)
                .opaque()
                .render_ops(rows("before"))
                .build()],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        let damage = compositor.replace_surfaces(
            [RetainedSurface::builder(Uuid::from_u128(1), rect)
                .opaque()
                .render_ops(rows("after"))
                .build()],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(damage.rects(), &[DamageRect::new(10, 6, 6, 1)]);
    }

    #[test]
    fn identical_plugin_surface_frame_is_a_no_op() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let surface = RetainedSurface::builder(Uuid::from_u128(625), DamageRect::new(2, 2, 12, 2))
            .revision(1)
            .render_ops(vec![RenderOp::text_run(
                2,
                2,
                "unchanged",
                RenderStyle::new(),
            )])
            .build();
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            [surface.clone()],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        let damage =
            compositor.replace_surfaces([surface], viewport, DamageCoalescingPolicy::default());
        assert_eq!(damage, RetainedDamage::None);
        assert!(compositor.repaint_plan(&damage).is_empty());
    }

    #[test]
    fn replace_surfaces_removal_damages_vacated_bounds_and_drops_state() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            [surface(1, 7, 4, 0, 0, true)],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        let removed = compositor.replace_surfaces([], viewport, DamageCoalescingPolicy::default());

        assert_eq!(removed.rects(), &[DamageRect::new(7, 4, 4, 2)]);
        assert!(compositor.surfaces().is_empty());
        assert!(compositor.repaint_plan(&removed).is_empty());
    }

    #[test]
    fn replacement_damages_only_changed_retained_item_bounds() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let rect = DamageRect::new(0, 0, 40, 10);
        let first = vec![
            RenderOp::text_run(2, 2, "stable", RenderStyle::default()),
            RenderOp::text_run(2, 4, "before", RenderStyle::default()),
        ];
        let second = vec![
            RenderOp::text_run(2, 2, "stable", RenderStyle::default()),
            RenderOp::text_run(2, 4, "after", RenderStyle::default()),
        ];
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            [RetainedSurface::builder(Uuid::from_u128(700), rect)
                .render_ops(first)
                .build()],
            viewport,
            DamageCoalescingPolicy::default(),
        );
        let damage = compositor.replace_surfaces(
            [RetainedSurface::builder(Uuid::from_u128(700), rect)
                .render_ops(second)
                .build()],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(damage.rects(), &[DamageRect::new(2, 4, 6, 1)]);
    }

    #[test]
    fn replace_surfaces_render_op_geometry_changes_damage_old_and_new_bounds() {
        let viewport = DamageRect::new(0, 0, 80, 24);
        let rect = DamageRect::new(10, 5, 12, 3);
        let mut compositor = RetainedCompositor::new();
        let _ = compositor.replace_surfaces(
            [RetainedSurface::builder(Uuid::from_u128(1), rect)
                .opaque()
                .render_ops(vec![RenderOp::text_run(
                    10,
                    5,
                    "before",
                    RenderStyle::default(),
                )])
                .build()],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        let damage = compositor.replace_surfaces(
            [
                RetainedSurface::builder(Uuid::from_u128(1), DamageRect::new(20, 8, 12, 3))
                    .opaque()
                    .render_ops(vec![RenderOp::text_run(
                        20,
                        8,
                        "after",
                        RenderStyle::default(),
                    )])
                    .build(),
            ],
            viewport,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(damage.rects(), &[DamageRect::new(10, 5, 22, 6)]);
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
    fn frame_damage_from_retained_repaint_plan_marks_scene_surface_rects() {
        let pane_id = Uuid::from_u128(80);
        let surface_id = Uuid::from_u128(81);
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
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: false,
                pane_id: Some(pane_id),
            }],
        };
        let plan = vec![RetainedRepaintSurface {
            surface_id,
            rect: DamageRect::new(10, 5, 20, 10),
            layer: retained_layer_order(AttachLayer::Pane),
            z: 0,
            opaque: true,
            modal: false,
            opacity: RetainedOpacity::Opaque,
            clip_rect: None,
            interactive_regions: Vec::new(),
            damage: vec![DamageRect::new(13, 9, 3, 1)],
        }];

        let damage = frame_damage_from_retained_repaint_plan(
            &scene,
            &plan,
            DamageCoalescingPolicy::default(),
        );

        assert_eq!(
            damage.extension_surface_rects(surface_id),
            &[DamageRect::new(3, 4, 3, 1)]
        );
        assert_eq!(
            damage.content_surface_rects(pane_id),
            &[DamageRect::new(1, 2, 3, 1)]
        );
        assert!(!damage.vacated_surface_damaged(surface_id));
    }

    #[test]
    fn retained_damage_helpers_merge_absolute_rects_and_full_damage() {
        let viewport = DamageRect::new(0, 0, 10, 5);
        let policy = DamageCoalescingPolicy::default();
        let rect_damage = retained_damage_from_absolute_rects(
            [DamageRect::new(1, 1, 2, 1), DamageRect::new(2, 1, 2, 1)],
            viewport,
            policy,
        );
        assert_eq!(rect_damage.rects(), &[DamageRect::new(1, 1, 3, 1)]);

        let merged = merge_retained_damages(
            [
                rect_damage,
                RetainedDamage::Regions(vec![DamageRect::new(9, 4, 1, 1)]),
            ],
            viewport,
            policy,
        );
        assert_eq!(
            merged.rects(),
            &[DamageRect::new(1, 1, 3, 1), DamageRect::new(9, 4, 1, 1)]
        );

        let full = merge_retained_damages(
            [merged, RetainedDamage::Full { viewport }],
            viewport,
            policy,
        );
        assert_eq!(full, RetainedDamage::Full { viewport });
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
        frame_damage.mark_vacated_surface_rect(
            surface_id,
            DamageRect::new(18, 8, 2, 1),
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
            vec![
                DamageRect::new(10, 5, 2, 1),
                DamageRect::new(28, 13, 2, 1),
                DamageRect::new(13, 9, 3, 1),
            ]
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
