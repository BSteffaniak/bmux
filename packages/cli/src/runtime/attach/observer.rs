//! Explicit per-attach, read-only latest-snapshot channel.
//! The host must authorize access before receiving this observer. There is no registry.
use std::sync::Arc;

use bmux_attach_pipeline::RetainedCompositor;
use bmux_tui::geometry::Rect;
use bmux_tui::presentation::PreparedPresentation;
use tokio::sync::watch;
use uuid::Uuid;

#[path = "observer_transport.rs"]
pub mod transport;

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
    /// Immutable output shared with direct delivery; commitment still belongs
    /// to the consuming `PresentationState::present_prepared` transaction.
    pub prepared: Arc<PreparedPresentation>,
    pub surfaces: Vec<AttachSurfaceProvenance>,
    /// Native producers expose compositor hits/focus, but not logical selection
    /// or semantic region snapshots. Empty metadata must not imply support.
    /// Source text keyed exactly as committed logical fragment content IDs.
    pub selection_text: std::collections::BTreeMap<bmux_tui::selection::SelectionContentId, String>,
    pub unsupported_metadata: &'static [&'static str],
}

type RejectionHandler = dyn FnMut(Option<&str>) + Send + Sync;

type SnapshotDelivery = dyn FnMut(&AttachSnapshot) -> anyhow::Result<()> + Send + Sync;
type PreparedOutput = dyn FnMut(bmux_frontend_runtime::presentation::PreparedDelivery) -> anyhow::Result<()>
    + Send
    + Sync;

/// Single publisher, owned and dropped by one attach view.
pub struct AttachObserverPublisher {
    pub(super) pane_zoom_shared: Option<super::pane_zoom_native::SharedZoom>,
    epoch: Uuid,
    revision: u64,
    sender: watch::Sender<Option<Arc<AttachSnapshot>>>,
    rejection: Option<Box<RejectionHandler>>,
    last_rejection: Option<String>,
    delivery: Option<Box<SnapshotDelivery>>,
    prepared_delivery: Option<(u64, Box<PreparedOutput>)>,
    prepared_revision: u64,
    prepared_authority: Option<bmux_frontend_runtime::presentation::PresentationAuthority>,
    capture_interval: Option<std::time::Duration>,
    last_capture: Option<std::time::Instant>,
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
            pane_zoom_shared: None,
            epoch,
            revision: 0,
            sender,
            rejection: None,
            last_rejection: None,
            delivery: None,
            prepared_delivery: None,
            prepared_revision: 0,
            prepared_authority: None,
            capture_interval: None,
            last_capture: None,
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
        if let Some(shared) = &self.pane_zoom_shared
            && let Ok(mut shared) = shared.lock()
        {
            *shared = super::pane_zoom_native::CommittedZoom::default();
        }
        self.sender.send_replace(None);
    }
}

impl AttachObserverPublisher {
    pub(super) fn latest_committed(&self) -> Option<Arc<AttachSnapshot>> {
        self.sender.borrow().clone()
    }

    /// Construct a direct owned-output adapter without a browser observer.
    ///
    /// The receiver is intentionally absent: the callback accepts the complete
    /// canonical prepared scene synchronously. Returning success for queued work
    /// would incorrectly promote interaction authority before native output.
    #[must_use]
    pub fn for_prepared_delivery(
        viewport_generation: u64,
        delivery: impl FnMut(
            bmux_frontend_runtime::presentation::PreparedDelivery,
        ) -> anyhow::Result<()>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let (mut publisher, _) = attach_observer_channel();
        publisher.set_prepared_delivery(viewport_generation, delivery);
        publisher
    }

    /// Replace output with a direct prepared-scene sink for the given viewport.
    ///
    /// Reinstall on viewport generation changes. The native owner must validate
    /// identity, generation and latest revision at actual output acceptance
    /// (for example with `PresentationAuthority`), not merely at enqueue time.
    pub fn set_prepared_delivery(
        &mut self,
        viewport_generation: u64,
        delivery: impl FnMut(
            bmux_frontend_runtime::presentation::PreparedDelivery,
        ) -> anyhow::Result<()>
        + Send
        + Sync
        + 'static,
    ) {
        self.delivery = None;
        self.prepared_delivery = Some((viewport_generation, Box::new(delivery)));
    }
    pub(super) fn set_max_fps(&mut self, fps: u8) {
        self.capture_interval = Some(std::time::Duration::from_nanos(
            1_000_000_000_u64.div_ceil(u64::from(fps.clamp(1, 60))),
        ));
    }

    pub(super) const fn has_published(&self) -> bool {
        self.revision != 0
    }

    pub(super) fn capture_deadline(&self) -> Option<std::time::Instant> {
        self.last_capture
            .zip(self.capture_interval)
            .map(|(last, interval)| last + interval)
    }

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

    /// Update the projection generation before capturing a resized native frame.
    ///
    /// The attach owner serializes this with synchronous delivery. Generations
    /// never decrease, including resize rollback; equal values are idempotent.
    ///
    /// # Errors
    /// Returns an error if no direct sink exists or the generation would regress.
    pub fn set_prepared_viewport_generation(&mut self, generation: u64) -> anyhow::Result<()> {
        let Some((current, _)) = &mut self.prepared_delivery else {
            anyhow::bail!("no direct prepared output installed");
        };
        if generation < *current {
            anyhow::bail!("prepared viewport generation regressed");
        }
        *current = generation;
        Ok(())
    }

    /// Identity of the last acknowledged direct output for the active binding.
    #[must_use]
    pub fn committed_prepared_identity(
        &self,
    ) -> Option<bmux_frontend_runtime::presentation::PresentationIdentity> {
        let (generation, _) = self.prepared_delivery.as_ref()?;
        self.prepared_authority
            .as_ref()?
            .committed()
            .filter(|delivery| delivery.identity.viewport_generation == *generation)
            .map(|delivery| delivery.identity)
    }

    /// Install synchronous bounded host admission/delivery before frame commit.
    pub fn set_delivery(
        &mut self,
        delivery: impl FnMut(&AttachSnapshot) -> anyhow::Result<()> + Send + Sync + 'static,
    ) {
        self.prepared_delivery = None;
        self.delivery = Some(Box::new(delivery));
    }
    pub(super) fn publish(
        &mut self,
        attached_id: Uuid,
        prepared: impl Into<Arc<PreparedPresentation>>,
        graph: &RetainedCompositor,
        stream_ends: &std::collections::BTreeMap<Uuid, Option<u64>>,
        pointer_shape: Option<bmux_attach_pipeline::pointer_shape::ResolvedPointerShape>,
    ) -> anyhow::Result<()> {
        let Some(revision) = self.revision.checked_add(1) else {
            anyhow::bail!("attach presentation revision exhausted");
        };
        let prepared = prepared.into();
        let surfaces = surface_provenance(graph, stream_ends);
        if let Some((viewport_generation, delivery)) = &mut self.prepared_delivery {
            use bmux_frontend_runtime::presentation::{
                PreparedDelivery, PresentationAuthority, PresentationIdentity,
            };
            self.prepared_revision = self
                .prepared_revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("prepared delivery revision exhausted"))?;
            let candidate = PreparedDelivery {
                identity: PresentationIdentity {
                    epoch: self.epoch,
                    revision: self.prepared_revision,
                    attached_id,
                    viewport_generation: *viewport_generation,
                },
                prepared: Arc::clone(&prepared),
                pointer_shape,
            };
            // These bindings come from the serialized attach owner, not an
            // incoming transport frame. The UI independently validates its live
            // viewport before acknowledging output.
            let authority = self.prepared_authority.get_or_insert_with(|| {
                PresentationAuthority::new(
                    self.epoch,
                    attached_id,
                    *viewport_generation,
                    prepared.buffer().area(),
                )
            });
            authority.rebind(
                self.epoch,
                attached_id,
                *viewport_generation,
                prepared.buffer().area(),
            );
            authority.stage(&candidate)?;
            authority
                .present_with_grid_fragments(candidate, |accepted| delivery(accepted.clone()))
                .map_err(|error| match error {
                    bmux_frontend_runtime::presentation::DeliveryError::Rejected(reason) => {
                        anyhow::Error::new(reason)
                    }
                    bmux_frontend_runtime::presentation::DeliveryError::Output(error) => error,
                })?;
        }
        let snapshot = Arc::new(AttachSnapshot {
            epoch: self.epoch,
            revision,
            attached_id,
            selection_text: prepared.selection_text().clone(),
            unsupported_metadata: unsupported_metadata(graph),
            prepared,
            surfaces,
        });
        if let Some(delivery) = &mut self.delivery {
            delivery(&snapshot)?;
        }
        self.sender.send_replace(Some(snapshot));
        self.revision = revision;
        self.last_capture = Some(std::time::Instant::now());
        self.report_rejection(None);
        Ok(())
    }
}

fn surface_provenance(
    graph: &RetainedCompositor,
    stream_ends: &std::collections::BTreeMap<Uuid, Option<u64>>,
) -> Vec<AttachSurfaceProvenance> {
    graph
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
        .collect()
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

fn unsupported_metadata(graph: &RetainedCompositor) -> &'static [&'static str] {
    let selection = graph.surfaces().values().any(|surface| {
        surface
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.selection.is_some())
    });
    let semantics = graph.surfaces().values().any(|surface| {
        surface
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.semantics.is_some())
    });
    match (selection, semantics) {
        (true, true) => &[],
        (true, false) => &["semantics"],
        (false, true) => &["selection"],
        (false, false) => &["selection", "semantics"],
    }
}

/// Reuse the accepted prepared scene for a pointer-only transition. There is no
/// second composition authority and no terminal byte output on this path.
pub(super) fn deliver_pointer_shape(
    view: &mut super::state::AttachViewState,
) -> anyhow::Result<()> {
    let Some(publisher) = view.observer.as_mut() else {
        anyhow::bail!("pointer delivery requires a committed presentation publisher");
    };
    let Some(snapshot) = publisher.latest_committed() else {
        anyhow::bail!("pointer delivery requires a successfully presented scene");
    };
    let stream_ends = view
        .pane_buffers
        .iter()
        .map(|(id, buffer)| (*id, buffer.expected_stream_start))
        .collect();
    publisher.publish(
        view.attached_id,
        Arc::clone(&snapshot.prepared),
        &view.retained_compositor,
        &stream_ends,
        view.pointer_shape,
    )?;
    if let Some(layout) = view.pointer_layout.clone() {
        super::pane_zoom_native::commit(view, &layout);
    }
    Ok(())
}

pub(super) fn deliver_prepared(
    view: &mut super::state::AttachViewState,
    geometry: super::input::TerminalGeometry,
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
    let result = deliver_prepared_inner(view, geometry, || {
        let result = output();
        output_failed = result.is_err();
        result
    });
    if result.is_err() {
        view.frontend_presentations.failed();
    }
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
                "browser frame rejected (viewport={}x{}): {error:#}",
                geometry.cols, geometry.rows
            ))
            .into()
        }
    })
}

#[allow(clippy::too_many_lines)] // Admission and successful-output commit stay one ordered transaction.
fn deliver_prepared_inner(
    view: &mut super::state::AttachViewState,
    geometry: super::input::TerminalGeometry,
    output: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use bmux_tui::{image::ImageContribution, presentation::PresentationLimits};
    let area = Rect::new(0, 0, geometry.cols, geometry.rows);
    let images = mirrored_images(view);
    let interactions =
        projected_interactions(&view.retained_compositor, area).map_err(anyhow::Error::msg)?;
    view.presentation.resize(area);
    let limits = PresentationLimits {
        max_cells: 262_144,
        max_entries: 262_144,
        max_bytes: 32 * 1024 * 1024,
    };
    let mut source_metadata = view
        .pane_zoom
        .prepared
        .iter()
        .map(|(id, projection)| projection.source_metadata().map(|metadata| (*id, metadata)))
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()
        .map_err(|error| anyhow::anyhow!("zoom metadata source rejected: {error:?}"))?;
    for (id, projection) in &view.pane_zoom.prepared_history {
        source_metadata.insert(
            *id,
            projection
                .source_metadata()
                .map_err(|error| anyhow::anyhow!("history metadata source rejected: {error:?}"))?,
        );
    }
    let frontends = view.frontend_presentations.prepared()?;
    let mut metadata =
        super::metadata::project_with_sources(&view.retained_compositor, area, &source_metadata)?;
    let modal_scopes = merge_frontend_metadata(&frontends, &mut metadata.text)?;
    let zoom_grids = super::pane_zoom_native::fragments(view)?;
    // The frame the client's terminal presenter shows is kept here by applying the change the
    // terminal made (its moves and changed cells): both hold one frame.
    let shown = view.terminal_presenter.shown_changes();
    anyhow::ensure!(
        !matches!(
            shown,
            bmux_attach_pipeline::frame_presenter::ShownChanges::Unknown
        ),
        "no presented frame to deliver"
    );
    let mut prepared = view
        .presentation
        .prepare_repaired_bounded(limits, |cx| {
            bmux_attach_pipeline::frame_presenter::paint_shown_changes(cx, &shown);
            for image in images {
                cx.push_image(ImageContribution::Present(image));
            }
            // Root output owns chrome only in winning local-grid clips. Clear
            // retained uniform text there so scale reset/removal cannot expose
            // a second, stale copy of the source below the fragment.
            for grid in &zoom_grids {
                for clip in grid.visible_clips() {
                    let area = bmux_tui::paint::LocalRect::new(i32::from(clip.x),i64::from(clip.y),clip.width,clip.height);
                    cx.occlude(area);
                    cx.fill(area, " ", bmux_tui::style::Style::new());
                    // Root metadata was occluded above; matching projected metadata
                    // is installed below in the same presentation transaction.
                }
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
            for (placement, frontend) in &frontends {
                cx.with_child(i32::from(placement.x), i64::from(placement.y),
                    bmux_tui::paint::LocalRect::terminal(Rect::new(0, 0, placement.width, placement.height)),
                    |cx| frontend.paint(cx));
            }
            if let Some(cursor) = view.last_cursor_state {
                cx.set_cursor_local(cursor.x, cursor.y, cursor.visible);
            }
        })
        .map_err(|error| anyhow::anyhow!("presentation admission failed: resource={error:?}, cells={}, cell_limit={}, entry_limit={}, byte_limit={}", usize::from(area.width) * usize::from(area.height), limits.max_cells, limits.max_entries, limits.max_bytes))?;
    prepared
        .set_grid_fragments(zoom_grids, limits)
        .map_err(|error| anyhow::anyhow!("zoom fragment admission failed: {error:?}"))?;
    anyhow::ensure!(
        prepared.set_modal_scopes(&modal_scopes),
        "frontend modal declarations require coherent full output"
    );
    prepared
        .set_selection_text(metadata.text)
        .map_err(|error| anyhow::anyhow!("logical source admission failed: {error:?}"))?;
    prepared
        .set_projected_metadata(
            bmux_tui::presentation::ProjectedMetadata {
                selection: metadata.selection_bounds,
                semantics: metadata.semantic_bounds,
            },
            limits,
        )
        .map_err(|error| anyhow::anyhow!("projected metadata admission failed: {error:?}"))?;
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
    commit_observed_presentation(view, prepared, output)
}

/// Deliver the prepared scene before acknowledging frontend interaction authority.
fn commit_observed_presentation(
    view: &mut super::state::AttachViewState,
    prepared: bmux_tui::presentation::PreparedPresentation,
    output: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
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
    let pointer_shape = view.pointer_shape;
    let displayed = view
        .presentation
        .present_prepared(Some(prepared), |frame, _| {
            output()?;
            publisher.publish(attached, frame.clone(), graph, &stream_ends, pointer_shape)
        });
    finish_frontend_display(&mut view.frontend_presentations, displayed)
}

/// Resource input authority advances only after the shared compositor output
/// succeeds with an actual frame. A skipped presentation acknowledges nothing;
/// any commit error revokes all affected frontend input leases.
fn finish_frontend_display(
    frontends: &mut super::frontend_presentation::FrontendPresentations,
    displayed: anyhow::Result<Option<()>>,
) -> anyhow::Result<()> {
    let result = match displayed {
        Ok(Some(())) => frontends.displayed(),
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    if result.is_err() {
        frontends.failed();
    }
    result
}

fn merge_frontend_metadata(
    frontends: &[(
        Rect,
        bmux_plugin::component_viewport::portable::PreparedFrontendPresentation,
    )],
    text: &mut std::collections::BTreeMap<bmux_tui::selection::SelectionContentId, String>,
) -> anyhow::Result<Vec<bmux_tui::focus::FocusScopeId>> {
    let mut scopes = Vec::new();
    for (_, frontend) in frontends {
        for (id, source) in frontend.selection_text() {
            anyhow::ensure!(
                text.insert(id, source).is_none(),
                "duplicate frontend logical source identity"
            );
        }
        scopes.extend(frontend.modal_scopes());
    }
    Ok(scopes)
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

/// The images the client's presenter shows, as one frame's images: what the observer shows,
/// except the pane images of surfaces it shows natively zoomed (their fragments carry them).
fn mirrored_images(view: &super::state::AttachViewState) -> Vec<bmux_tui::image::ImagePlacement> {
    let zoomed = view
        .pane_zoom
        .prepared
        .keys()
        .chain(view.pane_zoom.prepared_history.keys())
        .map(|surface| super::pane_images::surface_key_prefix(*surface))
        .collect::<Vec<_>>();
    view.terminal_presenter
        .image_scene()
        .placements()
        .filter(|image| {
            !zoomed
                .iter()
                .any(|prefix| image.key.as_str().starts_with(prefix.as_str()))
        })
        .map(|image| bmux_tui::image::ImagePlacement {
            lifecycle: bmux_tui::image::ImageLifecycle::Frame,
            ..image.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::{damage::Damage, presentation::PresentationState};

    #[test]
    fn native_and_observer_share_prepared_payload_through_pointer_republication() {
        use bmux_attach_pipeline::pointer_shape::ResolvedPointerShape;
        use bmux_tui::{geometry::Point, hit::HitRegion, style::Style};
        use std::sync::Mutex;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&seen);
        let (mut publisher, observer) = attach_observer_channel();
        publisher.set_prepared_delivery(1, move |delivery| {
            let mut seen = collected.lock().unwrap();
            seen.push(Arc::clone(&delivery.prepared));
            let rejected = seen.len() == 3;
            drop(seen);
            if rejected {
                anyhow::bail!("output rejected");
            }
            Ok(())
        });
        let area = Rect::new(0, 0, 2, 1);
        let mut state = PresentationState::new(area);
        let prepared = state
            .prepare(Damage::Full, |cx| {
                cx.set_cell(0, 0, "A", Style::default());
                cx.push_hit(HitRegion::new("committed", area));
            })
            .unwrap();
        let graph = RetainedCompositor::new();
        let ends = std::collections::BTreeMap::new();
        state
            .present_prepared(Some(prepared), |frame, _| {
                publisher.publish(Uuid::nil(), frame.clone(), &graph, &ends, None)
            })
            .unwrap();
        let first = observer.latest().unwrap();
        assert!(Arc::ptr_eq(&first.prepared, &seen.lock().unwrap()[0]));
        publisher
            .publish(
                Uuid::nil(),
                Arc::clone(&first.prepared),
                &graph,
                &ends,
                Some(ResolvedPointerShape::Hidden),
            )
            .unwrap();
        let pointer_update = observer.latest().unwrap();
        assert!(Arc::ptr_eq(&first.prepared, &pointer_update.prepared));
        assert!(Arc::ptr_eq(
            &pointer_update.prepared,
            &seen.lock().unwrap()[1]
        ));
        let rejected = state.prepare(Damage::Full, |_| {}).unwrap();
        assert!(
            state
                .present_prepared(Some(rejected), |frame, _| {
                    publisher.publish(Uuid::nil(), frame.clone(), &graph, &ends, None)
                })
                .is_err()
        );
        assert!(Arc::ptr_eq(&pointer_update, &observer.latest().unwrap()));
        assert!(state.hits().hit_test(Point::new(0, 0)).is_some());
        assert_eq!(pointer_update.prepared.buffer().cells()[0].symbol, "A");
        assert!(Arc::ptr_eq(
            &publisher
                .prepared_authority
                .as_ref()
                .unwrap()
                .committed()
                .unwrap()
                .prepared,
            &pointer_update.prepared
        ));
    }

    /// CPU-only warmed retained preparation, publication, and clone costs.
    /// Does not measure native queues, GPU output, or terminal encoding.
    #[test]
    #[ignore = "display-free performance measurements; run explicitly with --nocapture"]
    fn warmed_prepare_and_observer_clone_perf() {
        use bmux_tui::{hit::HitRegion, style::Style};
        use std::{hint::black_box, time::Instant};
        for (scenario, width, height) in [("small", 80, 24), ("large", 240, 80)] {
            for repair in [false, true] {
                let area = Rect::new(0, 0, width, height);
                let mut state = PresentationState::new(area);
                let (mut publisher, observer) = attach_observer_channel();
                publisher.set_prepared_delivery(1, |delivery| {
                    black_box(delivery.prepared.buffer().cells().len());
                    Ok(())
                });
                let graph = RetainedCompositor::new();
                let ends = std::collections::BTreeMap::new();
                let mut samples = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
                for iteration in 0..120 {
                    let start = Instant::now();
                    let render = |cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
                        // Full metadata replacement even when cell operations are sparse.
                        for y in 0..height {
                            cx.push_hit(HitRegion::new(
                                format!("row-{y}"),
                                Rect::new(0, y, width, 1),
                            ));
                            if !repair || iteration == 0 || y == 0 {
                                cx.set_cell(0, i64::from(y), "界", Style::default());
                            }
                        }
                    };
                    let prepared = if repair {
                        state
                            .prepare_repaired_bounded(
                                bmux_tui::presentation::PresentationLimits {
                                    max_cells: 262_144,
                                    max_entries: 262_144,
                                    max_bytes: 32 * 1024 * 1024,
                                },
                                render,
                            )
                            .unwrap()
                    } else {
                        state.prepare(Damage::Full, render).unwrap()
                    };
                    let prepare_ns = start.elapsed().as_nanos();
                    let start = Instant::now();
                    let copied = black_box(prepared.clone());
                    let deep_clone_ns = start.elapsed().as_nanos();
                    drop(copied);
                    let start = Instant::now();
                    state
                        .present_prepared(Some(prepared), |frame, _| {
                            publisher.publish(Uuid::nil(), frame.clone(), &graph, &ends, None)
                        })
                        .unwrap();
                    let publish_commit_ns = start.elapsed().as_nanos();
                    let snapshot = observer.latest().unwrap();
                    let start = Instant::now();
                    let shared = black_box(Arc::clone(&snapshot.prepared));
                    let arc_clone_ns = start.elapsed().as_nanos();
                    assert!(Arc::ptr_eq(&shared, &snapshot.prepared));
                    assert!(state.retained_buffer().is_some());
                    assert_eq!(
                        snapshot.prepared.hits().regions().len(),
                        usize::from(height)
                    );
                    if iteration >= 20 {
                        for (samples, elapsed) in samples.iter_mut().zip([
                            prepare_ns,
                            deep_clone_ns,
                            publish_commit_ns,
                            arc_clone_ns,
                        ]) {
                            samples.push(elapsed);
                        }
                    }
                }
                for (phase, mut samples) in
                    ["prepare", "deep_clone", "publish_and_commit", "arc_clone"]
                        .into_iter()
                        .zip(samples)
                {
                    samples.sort_unstable();
                    println!(
                        "scenario={scenario} repair={repair} phase={phase} samples={} p50_ns={} p99_ns={}",
                        samples.len(),
                        samples[samples.len() / 2],
                        samples[samples.len() * 99 / 100],
                    );
                }
            }
        }
    }

    #[test]
    fn direct_delivery_failure_preserves_observer_and_uses_fresh_attempt_identity() {
        use std::sync::Mutex;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let collected = seen.clone();
        let (mut publisher, observer) = attach_observer_channel();
        publisher.set_prepared_delivery(7, move |delivery| {
            let mut seen = collected.lock().unwrap();
            seen.push(delivery.identity);
            let reject = seen.len() == 2;
            drop(seen);
            if reject {
                anyhow::bail!("surface lost");
            }
            Ok(())
        });
        let prepared = PresentationState::new(Rect::new(0, 0, 2, 1))
            .prepare(Damage::Full, |_| {})
            .unwrap();
        let graph = RetainedCompositor::new();
        let target = Uuid::new_v4();
        let ends = std::collections::BTreeMap::new();
        publisher
            .publish(target, prepared.clone(), &graph, &ends, None)
            .unwrap();
        let committed = observer.latest().unwrap();
        assert!(
            publisher
                .publish(target, prepared.clone(), &graph, &ends, None)
                .is_err()
        );
        assert!(Arc::ptr_eq(&committed, &observer.latest().unwrap()));
        assert_eq!(publisher.committed_prepared_identity().unwrap().revision, 1);
        publisher
            .publish(target, prepared, &graph, &ends, None)
            .unwrap();
        assert_eq!(publisher.committed_prepared_identity().unwrap().revision, 3);
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|id| id.revision).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(seen.iter().all(|id| id.epoch == observer.epoch()
            && id.attached_id == target
            && id.viewport_generation == 7));
        drop(seen);
        assert_eq!(observer.latest().unwrap().revision, 2);
        publisher.set_prepared_viewport_generation(8).unwrap();
        assert!(publisher.set_prepared_viewport_generation(7).is_err());
    }

    #[test]
    fn prepared_pointer_intent_is_typed_and_failed_delivery_is_not_committed() {
        use bmux_attach_pipeline::pointer_shape::ResolvedPointerShape;
        use bmux_terminal_protocol::pointer_shape::PointerShape;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let mut publisher = AttachObserverPublisher::for_prepared_delivery(1, move |delivery| {
            recorded.lock().unwrap().push(delivery.pointer_shape);
            anyhow::ensure!(
                delivery.pointer_shape != Some(ResolvedPointerShape::Hidden),
                "rejected pointer"
            );
            Ok(())
        });
        let prepared = PresentationState::new(Rect::new(0, 0, 1, 1))
            .prepare(Damage::Full, |_| {})
            .unwrap();
        let graph = RetainedCompositor::new();
        let ends = std::collections::BTreeMap::new();
        let explicit_default = Some(ResolvedPointerShape::Shape(PointerShape::Default));
        publisher
            .publish(
                Uuid::nil(),
                prepared.clone(),
                &graph,
                &ends,
                explicit_default,
            )
            .unwrap();
        let committed = publisher.committed_prepared_identity().unwrap();
        assert!(
            publisher
                .publish(
                    Uuid::nil(),
                    prepared.clone(),
                    &graph,
                    &ends,
                    Some(ResolvedPointerShape::Hidden)
                )
                .is_err()
        );
        assert_eq!(publisher.committed_prepared_identity().unwrap(), committed);
        publisher
            .publish(Uuid::nil(), prepared, &graph, &ends, None)
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![explicit_default, Some(ResolvedPointerShape::Hidden), None]
        );
    }

    #[test]
    fn direct_adapter_needs_no_receiver_and_browser_adapter_replaces_it() {
        let mut publisher = AttachObserverPublisher::for_prepared_delivery(1, |_| {
            panic!("replaced native output must not run");
        });
        publisher.set_delivery(|_| Ok(()));
        let prepared = PresentationState::new(Rect::new(0, 0, 1, 1))
            .prepare(Damage::Full, |_| {})
            .unwrap();
        publisher
            .publish(
                Uuid::nil(),
                prepared,
                &RetainedCompositor::new(),
                &std::collections::BTreeMap::new(),
                None,
            )
            .unwrap();
        assert_eq!(publisher.revision, 1);
    }

    #[test]
    fn capture_deadline_is_absent_until_successful_publish() {
        let (mut publisher, _) = attach_observer_channel();
        publisher.set_max_fps(60);
        assert!(publisher.capture_deadline().is_none());
        let now = std::time::Instant::now();
        publisher.last_capture = Some(now);
        assert_eq!(
            publisher.capture_deadline().unwrap() - now,
            std::time::Duration::from_nanos(16_666_667)
        );
    }

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
            key: None,
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
        assert_eq!(
            projected_interactions(&graph, area).unwrap(),
            [] as [bmux_tui::hit::HitRegion; 0]
        );
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
                    None,
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
