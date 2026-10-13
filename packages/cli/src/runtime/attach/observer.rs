//! Explicit per-attach, read-only latest-snapshot channel.
//! The host must authorize access before receiving this observer. There is no registry.
use std::sync::Arc;

use bmux_attach_pipeline::RetainedCompositor;
use bmux_tui::geometry::Rect;
use bmux_tui::presentation::PreparedPresentation;
use tokio::sync::watch;
use uuid::Uuid;

/// Provenance of a surface in the exact committed retained graph.
#[derive(Debug, Clone)]
pub struct AttachSurfaceProvenance {
    pub retained_id: Uuid,
    pub owner_plugin_id: Option<String>,
    pub owner_local_id: Option<String>,
    pub revision: Option<u64>,
    pub rect: Rect,
    pub clip: Rect,
    pub layer: i16,
    pub z: i32,
    pub content_id: Option<Uuid>,
    /// Whether this producer supplies a logical selection scene (including empty).
    pub selection_available: bool,
    /// Whether this producer supplies semantic descriptions (including empty).
    pub semantics_available: bool,
    /// Execution-scoped output watermark, not an execution-generation identifier.
    pub stream_end: Option<u64>,
}

/// A complete repaired frame; omitted surfaces/images are removed, never retained implicitly.
#[derive(Debug)]
pub struct AttachSnapshot {
    /// Unique observer lifetime, known before attach starts.
    pub epoch: Uuid,
    /// Monotonic successful presentation revision within this epoch.
    pub revision: u64,
    /// Authoritative attached target identity (may change during retarget).
    pub attached_id: Uuid,
    pub prepared: PreparedPresentation,
    pub surfaces: Vec<AttachSurfaceProvenance>,
    /// Native producers expose compositor hits/focus, but not logical selection
    /// or semantic region snapshots. Empty metadata must not imply support.
    /// Source text keyed exactly as committed logical fragment content IDs.
    pub selection_text: std::collections::BTreeMap<bmux_tui::selection::SelectionContentId, String>,
    pub unsupported_metadata: &'static [&'static str],
}

type RejectionHandler = dyn FnMut(Option<&str>) + Send + Sync;

type SnapshotDelivery = dyn FnMut(&AttachSnapshot) -> anyhow::Result<()> + Send + Sync;

/// Single publisher, owned and dropped by one attach view.
pub struct AttachObserverPublisher {
    epoch: Uuid,
    revision: u64,
    sender: watch::Sender<Option<Arc<AttachSnapshot>>>,
    rejection: Option<Box<RejectionHandler>>,
    last_rejection: Option<String>,
    delivery: Option<Box<SnapshotDelivery>>,
}

impl std::fmt::Debug for AttachObserverPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachObserverPublisher")
            .field("epoch", &self.epoch)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// Latest-only observer; skipped updates are safe because every value is a full snapshot.
#[derive(Debug, Clone)]
pub struct AttachObserver {
    epoch: Uuid,
    receiver: watch::Receiver<Option<Arc<AttachSnapshot>>>,
}

/// Allocate an explicit observer before starting the selected attach.
#[must_use]
pub fn attach_observer_channel() -> (AttachObserverPublisher, AttachObserver) {
    let epoch = Uuid::new_v4();
    let (sender, receiver) = watch::channel(None);
    (
        AttachObserverPublisher {
            epoch,
            revision: 0,
            sender,
            rejection: None,
            last_rejection: None,
            delivery: None,
        },
        AttachObserver { epoch, receiver },
    )
}

impl AttachObserver {
    #[must_use]
    pub const fn epoch(&self) -> Uuid {
        self.epoch
    }
    #[must_use]
    pub fn latest(&self) -> Option<Arc<AttachSnapshot>> {
        if self.receiver.has_changed().is_err() {
            None
        } else {
            self.receiver.borrow().clone()
        }
    }
    /// Await another full snapshot. Closure means the explicit attach lifetime ended.
    ///
    /// # Errors
    /// Returns an error when the attach publisher has been dropped.
    pub async fn changed(&mut self) -> Result<Arc<AttachSnapshot>, watch::error::RecvError> {
        loop {
            self.receiver.changed().await?;
            let latest = self.receiver.borrow_and_update().clone();
            if let Some(snapshot) = latest {
                return Ok(snapshot);
            }
        }
    }
}

impl Drop for AttachObserverPublisher {
    fn drop(&mut self) {
        self.sender.send_replace(None);
    }
}

impl AttachObserverPublisher {
    pub(crate) fn set_rejection(
        &mut self,
        handler: impl FnMut(Option<&str>) + Send + Sync + 'static,
    ) {
        self.rejection = Some(Box::new(handler));
    }
    pub(super) fn report_rejection(&mut self, message: Option<&str>) {
        if self.last_rejection.as_deref() == message {
            return;
        }
        if let Some(handler) = &mut self.rejection {
            handler(message);
        }
        self.last_rejection = message.map(str::to_owned);
    }

    /// Install synchronous bounded host admission/delivery before frame commit.
    pub fn set_delivery(
        &mut self,
        delivery: impl FnMut(&AttachSnapshot) -> anyhow::Result<()> + Send + Sync + 'static,
    ) {
        self.delivery = Some(Box::new(delivery));
    }
    pub(super) fn publish(
        &mut self,
        attached_id: Uuid,
        prepared: PreparedPresentation,
        graph: &RetainedCompositor,
        stream_ends: &std::collections::BTreeMap<Uuid, Option<u64>>,
    ) -> anyhow::Result<()> {
        let Some(revision) = self.revision.checked_add(1) else {
            anyhow::bail!("attach presentation revision exhausted");
        };
        let surfaces = graph
            .ordered_surfaces()
            .into_iter()
            .map(|surface| {
                let rect = Rect::new(
                    surface.rect.x,
                    surface.rect.y,
                    surface.rect.w,
                    surface.rect.h,
                );
                let clip = surface
                    .paint_rect()
                    .map_or(Rect::new(0, 0, 0, 0), |r| Rect::new(r.x, r.y, r.w, r.h));
                AttachSurfaceProvenance {
                    retained_id: surface.id,
                    owner_plugin_id: surface
                        .owner
                        .as_ref()
                        .map(|owner| owner.owner_plugin_id.clone()),
                    owner_local_id: surface.owner.as_ref().map(|owner| owner.local_id.clone()),
                    revision: surface.revision,
                    rect,
                    clip,
                    selection_available: surface
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.selection.is_some()),
                    semantics_available: surface
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.semantics.is_some()),
                    layer: surface.layer,
                    z: surface.z,
                    stream_end: match &surface.payload {
                        bmux_attach_pipeline::RetainedSurfacePayload::Content { content_id } => {
                            stream_ends.get(content_id).copied().flatten()
                        }
                        _ => None,
                    },
                    content_id: match &surface.payload {
                        bmux_attach_pipeline::RetainedSurfacePayload::Content { content_id } => {
                            Some(*content_id)
                        }
                        _ => None,
                    },
                }
            })
            .collect();
        let snapshot = Arc::new(AttachSnapshot {
            epoch: self.epoch,
            revision,
            attached_id,
            selection_text: prepared.selection_text().clone(),
            unsupported_metadata: match (
                graph.surfaces().values().any(|surface| {
                    surface
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.selection.is_some())
                }),
                graph.surfaces().values().any(|surface| {
                    surface
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.semantics.is_some())
                }),
            ) {
                (true, true) => &[],
                (true, false) => &["semantics"],
                (false, true) => &["selection"],
                (false, false) => &["selection", "semantics"],
            },
            prepared,
            surfaces,
        });
        if let Some(delivery) = &mut self.delivery {
            delivery(&snapshot)?;
        }
        self.sender.send_replace(Some(snapshot));
        self.revision = revision;
        self.report_rejection(None);
        Ok(())
    }
}

/// A rejected complete browser frame, not a transport/process failure.
#[derive(Debug)]
pub(super) struct PresentationRejected(pub String);
impl std::fmt::Display for PresentationRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PresentationRejected {}

pub(super) fn deliver_prepared(
    view: &mut super::state::AttachViewState,
    layout: &bmux_client::AttachLayoutState,
    geometry: super::input::TerminalGeometry,
    operations: &[bmux_attach_pipeline::projection::PaintOperation],
    output: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let area = Rect::new(0, 0, geometry.cols, geometry.rows);
    let previous = (view.presentation.area() != area).then(|| {
        std::mem::replace(
            &mut view.presentation,
            bmux_tui::presentation::PresentationState::new(area),
        )
    });
    let mut output_failed = false;
    let result = deliver_prepared_inner(view, layout, geometry, operations, || {
        let result = output();
        output_failed = result.is_err();
        result
    });
    if result.is_err()
        && let Some(previous) = previous
    {
        view.presentation = previous;
    }
    result.map_err(|error| {
        if output_failed {
            error
        } else {
            PresentationRejected(format!(
                "browser frame rejected (viewport={}x{}, captured_operations={}): {error:#}",
                geometry.cols,
                geometry.rows,
                operations.len()
            ))
            .into()
        }
    })
}

fn deliver_prepared_inner(
    view: &mut super::state::AttachViewState,
    layout: &bmux_client::AttachLayoutState,
    geometry: super::input::TerminalGeometry,
    operations: &[bmux_attach_pipeline::projection::PaintOperation],
    output: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use bmux_tui::{image::ImageContribution, presentation::PresentationLimits};
    for op in operations {
        match op {
            bmux_attach_pipeline::projection::PaintOperation::Rejected(reason) => {
                anyhow::bail!("{reason}")
            }
            bmux_attach_pipeline::projection::PaintOperation::Unsupported => anyhow::bail!(
                "unsupported legacy imperative capture (no neutral retained representation)"
            ),
            _ => {}
        }
    }
    let area = Rect::new(0, 0, geometry.cols, geometry.rows);
    let images = observed_images(view, layout, area).map_err(anyhow::Error::msg)?;
    let interactions =
        projected_interactions(&view.retained_compositor, area).map_err(anyhow::Error::msg)?;
    view.presentation.resize(area);
    let limits = PresentationLimits {
        max_cells: 262_144,
        max_entries: 262_144,
        max_bytes: 32 * 1024 * 1024,
    };
    let metadata = super::metadata::project(&view.retained_compositor, area)?;
    let mut prepared = view
        .presentation
        .prepare_repaired_bounded(limits, |cx| {
            bmux_attach_pipeline::projection::paint(cx, operations);
            for image in images {
                cx.push_image(ImageContribution::Present(image));
            }
            for hit in interactions {
                cx.push_hit(hit);
            }
            for scope in metadata.selection.scopes() {
                cx.push_selection_scope(scope.clone());
            }
            for fragment in metadata.selection.fragments() {
                cx.push_selection_fragment(fragment.clone());
            }
            for region in metadata.semantics.regions() {
                cx.push_semantic(region.clone());
            }
            if let Some(cursor) = view.last_cursor_state {
                cx.set_cursor_local(cursor.x, cursor.y, cursor.visible);
            }
        })
        .map_err(|error| anyhow::anyhow!("presentation admission failed: resource={error:?}, cells={}, cell_limit={}, entry_limit={}, byte_limit={}", usize::from(area.width) * usize::from(area.height), limits.max_cells, limits.max_entries, limits.max_bytes))?;
    prepared
        .set_selection_text(metadata.text)
        .map_err(|error| anyhow::anyhow!("logical source admission failed: {error:?}"))?;
    if view.retained_compositor.surfaces().len() > 4096 {
        anyhow::bail!(
            "surface budget exceeded: used={}, limit=4096",
            view.retained_compositor.surfaces().len()
        );
    }
    let active = view
        .plugin_focus
        .focused()
        .map(|id| bmux_tui::focus::FocusId::new(region_identity(id)));
    if !prepared.project_active_focus(active.as_ref()) {
        prepared.project_active_focus(None);
    }
    let publisher = view
        .observer
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("presentation output closed"))?;
    let stream_ends = view
        .pane_buffers
        .iter()
        .map(|(id, buffer)| (*id, buffer.expected_stream_start))
        .collect();
    let attached = view.attached_id;
    let graph = &view.retained_compositor;
    view.presentation
        .present_prepared(Some(prepared), |frame, _| {
            output()?;
            publisher.publish(attached, frame.clone(), graph, &stream_ends)
        })?;
    Ok(())
}

fn region_identity(id: &bmux_plugin::surface::PluginSurfaceRegionId) -> String {
    format!(
        "{}:{}:{}",
        id.owner_plugin_id, id.surface_local_id, id.region_local_id
    )
}

/// Rasterize the compositor's actual winning hits, not the raw region rectangles.
fn projected_interactions(
    graph: &RetainedCompositor,
    area: Rect,
) -> Result<Vec<bmux_tui::hit::HitRegion>, String> {
    use bmux_tui::hit::HitRegion;
    let regions = graph
        .surfaces()
        .values()
        .map(|surface| surface.interactive_regions.len())
        .sum::<usize>();
    let cells = usize::from(area.width) * usize::from(area.height);
    if graph.surfaces().len() > 128
        || regions > 1024
        || cells.saturating_mul(graph.surfaces().len().saturating_add(regions)) > 4_194_304
    {
        return Err("interaction projection work budget exceeded".into());
    }
    let mut hits = Vec::new();
    for y in area.y..area.bottom() {
        let winners = (area.x..area.right())
            .map(|x| graph.hit_test(x, y))
            .collect::<Vec<_>>();
        let mut x = area.x;
        while x < area.right() {
            let Some(hit) = winners[usize::from(x - area.x)].as_ref() else {
                x += 1;
                continue;
            };
            let start = x;
            x += 1;
            while x < area.right()
                && winners[usize::from(x - area.x)]
                    .as_ref()
                    .is_some_and(|next| {
                        next.surface_id == hit.surface_id && next.region_index == hit.region_index
                    })
            {
                x += 1;
            }
            let surface = &graph.surfaces()[&hit.surface_id];
            let region = &surface.interactive_regions[hit.region_index];
            let id = region.id.as_ref().map_or_else(
                || format!("{}:{}", surface.id, hit.region_index),
                region_identity,
            );
            let focusable =
                region.focusable && region.id.as_ref().is_some_and(|id| graph.focus_allowed(id));
            hits.push(
                HitRegion::new(id, Rect::new(start, y, x - start, 1))
                    .layer(surface.layer)
                    .focusable(focusable),
            );
            if hits.len() > 4096 {
                return Err("interaction span budget exceeded".into());
            }
        }
    }
    Ok(hits)
}

#[cfg(not(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
)))]
fn observed_images(
    _view: &super::state::AttachViewState,
    _layout: &bmux_client::AttachLayoutState,
    _area: Rect,
) -> Result<Vec<bmux_tui::image::ImagePlacement>, String> {
    Ok(Vec::new())
}

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
fn observed_images(
    view: &super::state::AttachViewState,
    layout: &bmux_client::AttachLayoutState,
    area: Rect,
) -> Result<Vec<bmux_tui::image::ImagePlacement>, String> {
    use bmux_tui::image::ImagePayload;
    let mut result = Vec::new();
    let mut budget = 0_usize;
    for surface in &layout.scene.surfaces {
        let Some(content_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        let content = Rect::new(
            surface.content_rect.x,
            surface.content_rect.y,
            surface.content_rect.w,
            surface.content_rect.h,
        );
        for image in view.presented_images(content_id) {
            let payload = observed_image_payload(image)?;
            let destination = Rect::new(
                content.x.saturating_add(image.position_col),
                content.y.saturating_add(image.position_row),
                image.cell_cols,
                image.cell_rows,
            );
            let clip = content.intersection(area);
            let incarnation = view
                .image_execution_incarnations
                .get(&content_id)
                .copied()
                .unwrap_or(Uuid::nil());
            let key = format!("{}:{}:{incarnation}:{}", surface.id, content_id, image.id);
            let strips = bmux_attach_pipeline::projection_images::visible_image_strips(
                &key,
                &payload,
                destination,
                clip,
                |x, y| {
                    view.retained_compositor
                        .point_visible_from(surface.id, x, y)
                },
            )?;
            for strip in strips {
                budget = budget.saturating_add(match &strip.payload {
                    ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => {
                        bytes.len()
                    }
                });
                if budget > 8 * 1024 * 1024 || result.len() >= 4096 {
                    return Err("full-view image budget exceeded".into());
                }
                result.push(strip);
            }
        }
    }
    Ok(result)
}

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
fn observed_image_payload(
    image: &bmux_attach_image_protocol::AttachPaneImage,
) -> Result<bmux_tui::image::ImagePayload, String> {
    use bmux_tui::image::ImagePayload;
    #[cfg(feature = "image-sixel")]
    use bmux_tui::image::ImagePixelFormat;

    if image.raw_data.len() > 16 * 1024 * 1024 {
        return Err("encoded image budget exceeded".into());
    }
    let native = bmux_image::PaneImage::from(image);
    match image.protocol {
        bmux_attach_image_protocol::AttachImageProtocol::KittyGraphics => {
            // The authoritative registry normalizes completed Kitty transfers to PNG.
            Ok(ImagePayload::Png {
                bytes: native.payload.raw.ok_or("missing Kitty image")?,
                width: image.pixel_width,
                height: image.pixel_height,
            })
        }
        bmux_attach_image_protocol::AttachImageProtocol::ITerm2 => {
            #[cfg(feature = "image-iterm2")]
            {
                let (_, bytes) = bmux_image::codec::iterm2::parse_body(
                    native.payload.raw.as_deref().ok_or("missing iTerm image")?,
                )
                .ok_or("invalid iTerm image")?;
                Ok(ImagePayload::Png {
                    bytes,
                    width: image.pixel_width,
                    height: image.pixel_height,
                })
            }
            #[cfg(not(feature = "image-iterm2"))]
            {
                Err("iTerm decoding disabled".into())
            }
        }
        bmux_attach_image_protocol::AttachImageProtocol::Sixel => {
            #[cfg(feature = "image-sixel")]
            {
                if u64::from(image.pixel_width) * u64::from(image.pixel_height)
                    > bmux_attach_pipeline::projection_images::MAX_IMAGE_PIXELS
                {
                    return Err("sixel pixel budget exceeded".into());
                }
                let pixels = bmux_image::codec::sixel::decode(
                    native.payload.raw.as_deref().ok_or("missing sixel image")?,
                )
                .ok_or("invalid sixel image")?;
                Ok(ImagePayload::Pixels {
                    bytes: pixels.data,
                    width: pixels.width,
                    height: pixels.height,
                    format: match pixels.format {
                        bmux_image::model::PixelFormat::Rgb8 => ImagePixelFormat::Rgb8,
                        bmux_image::model::PixelFormat::Rgba8 => ImagePixelFormat::Rgba8,
                        bmux_image::model::PixelFormat::Png => {
                            return Err("unexpected sixel PNG".into());
                        }
                    },
                })
            }
            #[cfg(not(feature = "image-sixel"))]
            {
                Err("sixel decoding disabled".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::{damage::Damage, presentation::PresentationState};

    #[test]
    fn interactions_use_winning_clipped_spans_and_exact_native_focus() {
        use bmux_attach_pipeline::{
            DamageCoalescingPolicy, DamageRect, RetainedInteractiveRegion, RetainedSurface,
        };
        use bmux_plugin::surface::{PluginSurfaceCursor, PluginSurfaceId, PluginSurfaceRegionId};
        use bmux_tui::{geometry::Point, hit::HitId};
        let area = Rect::new(0, 0, 8, 2);
        let owner = PluginSurfaceId::new("owner", "surface", Uuid::new_v4());
        let id = PluginSurfaceRegionId::new(&owner, "control");
        let endpoint = bmux_plugin::AttachInputEndpoint {
            capability: "test".into(),
            interface_id: "test".into(),
            operation: "test".into(),
        };
        let mut surface = RetainedSurface::new(
            owner.retained_id,
            DamageRect::new(1, 0, 4, 2),
            0,
            0,
            true,
            Vec::new(),
        );
        surface.clip_rect = Some(DamageRect::new(2, 0, 2, 2));
        surface.interactive_regions = vec![RetainedInteractiveRegion {
            id: Some(id),
            rect: DamageRect::new(0, 0, 8, 2),
            focusable: true,
            cursor: PluginSurfaceCursor::Pointer,
            endpoint: Some(endpoint),
        }];
        let mut graph = RetainedCompositor::new();
        graph.replace_surfaces(
            vec![surface.clone()],
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        let hits = projected_interactions(&graph, area).unwrap();
        assert!(
            hits.iter()
                .all(|hit| hit.area.x == 2 && hit.area.width == 2)
        );
        let state = PresentationState::new(area);
        let mut prepared = state
            .prepare(Damage::Full, |cx| {
                for hit in hits {
                    cx.push_hit(hit);
                }
            })
            .unwrap();
        assert!(prepared.project_active_focus(None));
        assert!(prepared.focus().active().is_none());
        let mut native_focus = bmux_attach_pipeline::RetainedFocusState::default();
        assert!(native_focus.focus_hit(&graph, &graph.hit_test(2, 0).unwrap()));
        native_focus.reconcile(&graph);
        let active = HitId::new(region_identity(native_focus.focused().unwrap()));
        assert!(prepared.project_active_focus(Some(&active)));
        assert_eq!(prepared.focus().active(), Some(&active));
        assert!(prepared.hits().hit_test(Point::new(1, 0)).is_none());
        let mut modal = RetainedSurface::new(
            Uuid::new_v4(),
            DamageRect::new(3, 0, 3, 2),
            1,
            0,
            true,
            Vec::new(),
        );
        modal.modal = true;
        graph.replace_surfaces(
            vec![surface, modal],
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        let hits = projected_interactions(&graph, area).unwrap();
        assert!(
            hits.iter()
                .all(|hit| hit.area.x == 2 && hit.area.width == 1 && !hit.focusable)
        );
        graph.replace_surfaces(
            Vec::new(),
            DamageRect::new(0, 0, 8, 2),
            DamageCoalescingPolicy::default(),
        );
        native_focus.reconcile(&graph);
        assert!(native_focus.focused().is_none());
        assert!(projected_interactions(&graph, area).unwrap().is_empty());
    }

    #[test]
    fn latest_replaces_pending_frames_and_closes_with_view() {
        let (mut publisher, observer) = attach_observer_channel();
        let state = PresentationState::new(Rect::new(0, 0, 2, 1));
        for _ in 0..3 {
            publisher
                .publish(
                    Uuid::nil(),
                    state.prepare(Damage::Full, |_| {}).unwrap(),
                    &RetainedCompositor::new(),
                    &std::collections::BTreeMap::new(),
                )
                .unwrap();
        }
        assert_eq!(observer.latest().unwrap().revision, 3);
        assert_eq!(observer.latest().unwrap().epoch, observer.epoch());
        drop(publisher);
        assert!(observer.latest().is_none());
        assert!(observer.receiver.has_changed().is_err());
    }
}
