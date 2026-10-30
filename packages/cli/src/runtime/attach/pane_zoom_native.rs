//! Native attach-local zoom intent and committed source correspondence.
use super::{
    pane_zoom::{PaneZoomProjection, PaneZoomSource},
    state::AttachViewState,
};
use bmux_attach_pipeline::{RetainedCompositor, compositor::RetainedSurfacePayload};
use bmux_frontend_runtime::presentation::PresentationIdentity;
use bmux_pane_runtime_plugin_api::{
    pane_runtime_execution_v4::TerminalExecutionRef, pane_runtime_view_size_v1,
};
use bmux_terminal_grid::{ContentPosition, GridMode};
use bmux_tui::{
    geometry::{Point, Rect},
    grid_fragment::{GridTransform, PreparedGridFragment},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Exact committed local pane-view binding, not an execution capability. Bterm
/// may retain this for an event, but dequeue validates it against current output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativePaneZoomTarget {
    pub identity: PresentationIdentity,
    pub pane_id: Uuid,
    pub view_id: Uuid,
    pub execution_id: Uuid,
    pub source_revision: u64,
    pub scale_millis: u16,
    /// Effective host raster cap; presentation intent must be clamped before enqueue.
    pub max_scale_millis: u16,
}

/// Nonblocking bound-source observation; contention is not source removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePaneZoomRefresh {
    Pending,
    Gone,
    Ready {
        target: NativePaneZoomTarget,
        progress_revision: u64,
    },
}

#[derive(Default)]
pub(super) struct CommittedZoom {
    pub identity: Option<PresentationIdentity>,
    pub graph: RetainedCompositor,
    pub targets: BTreeMap<Uuid, NativePaneZoomTarget>,
    pub focused: Option<Uuid>,
    pub progress_revision: u64,
    pub max_scale_millis: Option<u16>,
}
pub(super) type SharedZoom = Arc<Mutex<CommittedZoom>>;

impl CommittedZoom {
    pub fn target(&self, point: Option<[f64; 2]>) -> Option<NativePaneZoomTarget> {
        let pane = match point {
            Some(point) => self.graph.content_cell_at(point)?.0,
            None => {
                return self
                    .targets
                    .get(&self.focused?)
                    .copied()
                    .filter(|target| Some(target.identity) == self.identity);
            }
        };
        // Choose the winning source surface, not a second geometry/hit map.
        self.graph
            .ordered_surfaces()
            .into_iter()
            .rev()
            .find_map(|surface| {
                let RetainedSurfacePayload::Content { content_id } = surface.payload else {
                    return None;
                };
                if content_id != pane {
                    return None;
                }
                if let Some(point) = point {
                    let clips = self.graph.visible_content_clips(surface.id, 256)?;
                    if !clips.iter().any(|rect| {
                        point[0] >= f64::from(rect.x)
                            && point[1] >= f64::from(rect.y)
                            && point[0] < f64::from(rect.x) + f64::from(rect.w)
                            && point[1] < f64::from(rect.y) + f64::from(rect.h)
                    }) {
                        return None;
                    }
                }
                self.targets
                    .get(&surface.id)
                    .copied()
                    .filter(|target| Some(target.identity) == self.identity)
            })
    }
}

#[derive(Clone)]
pub(super) struct ZoomSelection {
    projection: Arc<PaneZoomProjection>,
    start: bmux_terminal_grid::ContentAnchor,
    end: bmux_terminal_grid::ContentAnchor,
    dragging: bool,
}

fn selection_contains(
    selection: Option<&ZoomSelection>,
    projection: &PaneZoomProjection,
    anchor: bmux_terminal_grid::ContentAnchor,
) -> bool {
    let Some(selection) = selection else {
        return false;
    };
    if selection.projection.capture_id != projection.capture_id
        || selection.projection.source != projection.source
    {
        return false;
    }
    let key = |anchor: bmux_terminal_grid::ContentAnchor| (anchor.line, anchor.column);
    let (start, end) = if key(selection.start) <= key(selection.end) {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    };
    key(anchor) >= key(start) && key(anchor) <= key(end)
}

pub(super) fn root_point(view: &AttachViewState, x: u16, y: u16) -> [f64; 2] {
    view.pane_zoom
        .shared_pointer_root
        .unwrap_or_else(|| [f64::from(x), f64::from(y)])
}

fn selection_hit(
    view: &AttachViewState,
    event: crossterm::event::MouseEvent,
) -> Option<(Arc<PaneZoomProjection>, bmux_terminal_grid::ContentAnchor)> {
    let point = root_point(view, event.column, event.row);
    let (pane, local) = view.retained_compositor.content_cell_at(point)?;
    if !local
        .iter()
        .all(|value| value.is_finite() && *value >= 0.0 && *value < 65536.0)
    {
        return None;
    }
    // Source correspondence is keyed by the retained surface, never a second
    // root rectangle map. A capture-local anchor has no authority on its own.
    let projection = view
        .pane_zoom
        .committed
        .iter()
        .find_map(|(id, projection)| {
            if projection.source.pane != pane {
                return None;
            }
            let clips = view.retained_compositor.visible_content_clips(*id, 256)?;
            clips
                .iter()
                .any(|rect| {
                    point[0] >= f64::from(rect.x)
                        && point[1] >= f64::from(rect.y)
                        && point[0] < f64::from(rect.x) + f64::from(rect.w)
                        && point[1] < f64::from(rect.y) + f64::from(rect.h)
                })
                .then_some(projection.clone())
        })?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // Finite bounded local coordinates checked above.
    let position = ContentPosition {
        column: local[0].floor() as usize,
        row: local[1].floor() as usize,
    };
    let anchor = projection.source_at(position).ok()?;
    Some((projection, anchor))
}

pub(super) fn begin_selection(
    view: &mut AttachViewState,
    pane: Uuid,
    event: crossterm::event::MouseEvent,
) -> bool {
    let Some((projection, anchor)) = selection_hit(view, event) else {
        return false;
    };
    if projection.source.pane != pane {
        return false;
    }
    view.pane_zoom.selection = Some(ZoomSelection {
        projection,
        start: anchor,
        end: anchor,
        dragging: true,
    });
    view.dirty
        .mark_full_frame(super::state::AttachDirtySource::Mouse);
    true
}

pub(super) fn drag_selection(
    view: &mut AttachViewState,
    event: crossterm::event::MouseEvent,
) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    if !matches!(
        event.kind,
        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
    ) || !view
        .pane_zoom
        .selection
        .as_ref()
        .is_some_and(|selection| selection.dragging)
    {
        return false;
    }
    let hit = selection_hit(view, event);
    let selection = view
        .pane_zoom
        .selection
        .as_mut()
        .expect("checked selection");
    if let Some((projection, anchor)) = hit
        && projection.source == selection.projection.source
        && projection.capture_id == selection.projection.capture_id
    {
        selection.end = anchor;
    }
    if matches!(event.kind, MouseEventKind::Up(_)) {
        selection.dragging = false;
    }
    view.dirty
        .mark_full_frame(super::state::AttachDirtySource::Mouse);
    true
}

pub(super) fn selected_text(view: &AttachViewState) -> Option<String> {
    let selection = view.pane_zoom.selection.as_ref()?;
    let current = view
        .pane_zoom
        .committed
        .get(&selection.projection.source.view)?;
    if current.capture_id != selection.projection.capture_id
        || current.source != selection.projection.source
    {
        return None;
    }
    let projection = &selection.projection;
    let mut text = String::new();
    let mut previous_line = None;
    for row in projection.selection_rows().ok()? {
        for cell in row.cells {
            let anchor = bmux_terminal_grid::ContentAnchor {
                column: row.source.start.column + cell.columns.start,
                ..row.source.start
            };
            if !selection_contains(Some(selection), projection, anchor) {
                continue;
            }
            if previous_line.is_some_and(|line| line != anchor.line) {
                text.push('\n');
            }
            text.push_str(
                &projection
                    .export_text(anchor.capture, anchor.line, cell.bytes)
                    .ok()?,
            );
            if text.len() > 1_048_576 {
                return None;
            }
            previous_line = Some(anchor.line);
        }
    }
    Some(text)
}

pub(super) fn expand_pointer(
    identity: PresentationIdentity,
    point: [f64; 2],
    event: super::runtime::NativePanePointer,
) -> Option<super::runtime::NativeAttachEvent> {
    use super::runtime::{NativeAttachEvent, NativePanePointer};
    if !point
        .iter()
        .all(|value| value.is_finite() && *value >= 0.0 && *value < 65536.0)
    {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // Finite root-cell bounds checked above.
    let (column, row) = (point[0].floor() as u16, point[1].floor() as u16);
    Some(match event {
        NativePanePointer::Mouse {
            button,
            phase,
            modifiers,
        } => NativeAttachEvent::Mouse {
            identity,
            column,
            row,
            button,
            phase,
            modifiers,
        },
        NativePanePointer::Motion { drag, modifiers } => NativeAttachEvent::Motion {
            identity,
            column,
            row,
            drag,
            modifiers,
        },
        NativePanePointer::Scroll {
            delta_x,
            delta_y,
            modifiers,
        } => NativeAttachEvent::Scroll {
            identity,
            column,
            row,
            delta_x,
            delta_y,
            modifiers,
        },
    })
}

#[derive(Default)]
pub(super) struct PaneZoomState {
    pub shared: Option<SharedZoom>,
    pub execution_tokens: BTreeMap<Uuid, Uuid>,
    pub requested: BTreeMap<Uuid, (PaneZoomSource, u16)>,
    pub(super) shared_pointer_root: Option<[f64; 2]>,
    pub selection: Option<ZoomSelection>,
    pub cursor_owners: std::collections::BTreeSet<Uuid>,
    pub prepared: BTreeMap<Uuid, Arc<PaneZoomProjection>>,
    pub committed: BTreeMap<Uuid, Arc<PaneZoomProjection>>,
}

pub(super) fn prepare_frame(
    view: &mut AttachViewState,
    layout: &bmux_client::AttachLayoutState,
    surfaces: &mut [bmux_attach_pipeline::RetainedSurface],
) {
    view.pane_zoom.cursor_owners = layout
        .scene
        .surfaces
        .iter()
        .filter(|surface| surface.cursor_owner)
        .map(|surface| surface.id)
        .collect();
    if let Err(error) = prepare(view, surfaces) {
        tracing::warn!(%error, "local zoom preparation rejected");
        // Reject the complete candidate instead of presenting an unzoomed fallback.
        bmux_attach_pipeline::projection::record(
            bmux_attach_pipeline::projection::PaintOperation::Rejected(
                bmux_attach_pipeline::projection::ProjectionFailure::Unsupported {
                    reason: "local zoom projection rejected",
                },
            ),
        );
    }
}

pub(super) fn prepare(
    view: &mut AttachViewState,
    surfaces: &mut [bmux_attach_pipeline::RetainedSurface],
) -> anyhow::Result<()> {
    view.pane_zoom.prepared.clear();
    let active = surfaces
        .iter()
        .map(|surface| surface.id)
        .collect::<std::collections::BTreeSet<_>>();
    view.pane_zoom
        .execution_tokens
        .retain(|pane, _| view.pane_buffers.contains_key(pane));
    view.pane_zoom.requested.retain(|id, _| active.contains(id));
    for buffer in view.pane_buffers.values_mut() {
        buffer.local_zoom.projected = false;
    }
    let mut retained_bytes = 0usize;
    for surface in surfaces {
        let RetainedSurfacePayload::Content { content_id: pane } = surface.payload else {
            continue;
        };
        if !view.pane_buffers.contains_key(&pane) {
            continue;
        }
        let Some(execution) = view.pane_zoom.execution_tokens.get(&pane).copied() else {
            continue;
        };
        let source = PaneZoomSource {
            view: surface.id,
            pane,
            execution,
        };
        let binding = view.pane_zoom.requested.get(&surface.id).copied();
        let (binding, scale) = match binding {
            Some(binding) => binding,
            None if view
                .pane_zoom
                .requested
                .values()
                .any(|(binding, _)| binding.pane == pane && binding.execution == execution) =>
            {
                (source, 1000)
            }
            None => continue,
        };
        if binding != source {
            view.pane_zoom.requested.remove(&surface.id);
            continue;
        }
        if let Some(buffer) = view.pane_buffers.get_mut(&pane) {
            buffer.local_zoom.preserve_execution_geometry = true;
        }
        // Once a local view opts into source-aware sizing, scale 1 is still a
        // local projection; resetting zoom must not resume replica mutation.
        let rect = surface.content_rect.unwrap_or(surface.rect);
        let transform = GridTransform::new(Point::new(rect.x, rect.y), scale)
            .ok_or_else(|| anyhow::anyhow!("invalid local zoom transform"))?;
        let columns = (usize::from(rect.w) * 1000 / usize::from(scale)).max(1);
        let rows = (usize::from(rect.h) * 1000 / usize::from(scale)).max(1);
        let Some(buffer) = view.pane_buffers.get_mut(&pane) else {
            continue;
        };
        if view.pane_scrollback.contains_key(&pane) {
            anyhow::bail!("local zoom scrollback projection unavailable");
        }
        let grid = buffer.terminal_grid.grid();
        let alternate = grid.mode() == GridMode::Alternate;
        let capture = view
            .pane_zoom
            .committed
            .get(&surface.id)
            .filter(|old| old.matches_source(source, grid))
            .map_or_else(Uuid::new_v4, |old| old.capture_id);
        let projection = Arc::new(
            (if alternate {
                PaneZoomProjection::capture_positioned(source, capture, grid)
            } else {
                PaneZoomProjection::capture(source, capture, grid, columns, rows)
            })
            .map_err(|error| anyhow::anyhow!("zoom source projection rejected: {error:?}"))?,
        );
        retained_bytes = retained_bytes.saturating_add(projection.retained_bytes());
        if view.pane_zoom.prepared.len() >= 32 || retained_bytes > 32 * 1024 * 1024 {
            anyhow::bail!("local zoom view budget exceeded");
        }
        *surface = surface.clone().with_content_transform(transform);
        buffer.local_zoom.projected = true;
        view.pane_zoom.prepared.insert(surface.id, projection);
    }
    Ok(())
}

pub(super) fn fragments(view: &AttachViewState) -> anyhow::Result<Vec<PreparedGridFragment>> {
    view.pane_zoom
        .prepared
        .iter()
        .map(|(id, projection)| {
            let surface = view
                .retained_compositor
                .surfaces()
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("zoom surface disappeared"))?;
            let transform = surface
                .content_transform
                .ok_or_else(|| anyhow::anyhow!("zoom transform missing"))?;
            let plan = view
                .retained_compositor
                .content_paint_plan(*id, 4096)
                .ok_or_else(|| anyhow::anyhow!("zoom visibility/provenance budget exceeded"))?;
            let overlays = bmux_attach_pipeline::projection::prepare_content_overlays(&plan)
                .map_err(|error| anyhow::anyhow!("zoom overlay projection: {error}"))?;
            let clips = plan
                .clips
                .into_iter()
                .map(|rect| Rect::new(rect.x, rect.y, rect.w, rect.h))
                .collect();
            let fragment = projection
                .grid_fragment_with_selection(
                    format!("pane-zoom:{id}"),
                    transform,
                    clips,
                    grid_style,
                    |anchor| {
                        selection_contains(view.pane_zoom.selection.as_ref(), projection, anchor)
                    },
                    view.pane_zoom.cursor_owners.contains(id)
                        && !view.retained_compositor.has_modal_scope(),
                )
                .ok_or_else(|| anyhow::anyhow!("zoom fragment unsupported"))?
                .with_overlays(overlays)
                .ok_or_else(|| anyhow::anyhow!("zoom overlay admission failed"))?;
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            {
                use bmux_tui::image::{ImageFit, ImageLifecycle, ImagePlacement};
                let mut images = Vec::new();
                let mut cells = 0usize;
                for image in view.presented_images(projection.source.pane) {
                    cells = cells.saturating_add(
                        usize::from(image.cell_cols) * usize::from(image.cell_rows),
                    );
                    anyhow::ensure!(cells <= 262_144, "zoom image mapping budget exceeded");
                    let destination = projection
                        .image_rect(Rect::new(
                            image.position_col,
                            image.position_row,
                            image.cell_cols,
                            image.cell_rows,
                        ))
                        .ok_or_else(|| {
                            anyhow::anyhow!("zoom image source rectangle unavailable")
                        })?;
                    images.push(Arc::new(ImagePlacement {
                        key: format!("zoom:{id}:{}:{}", projection.source.execution, image.id)
                            .into(),
                        payload: super::observer::observed_image_payload(image)
                            .map_err(anyhow::Error::msg)?,
                        destination,
                        fit: ImageFit::Contain,
                        allocation: None,
                        clip: destination,
                        lifecycle: ImageLifecycle::Persistent,
                    }));
                }
                fragment
                    .with_images(images)
                    .ok_or_else(|| anyhow::anyhow!("zoom image placement unavailable"))
            }
            #[cfg(not(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            )))]
            Ok(fragment)
        })
        .collect()
}

fn grid_style(style: bmux_terminal_grid::Style) -> bmux_tui::style::Style {
    use bmux_tui::style::{Color, Modifier, Style};
    let color = |value| match value {
        bmux_terminal_grid::Color::Indexed(index) => Color::Indexed(index),
        bmux_terminal_grid::Color::Rgb { r, g, b } => Color::Rgb(r, g, b),
    };
    let mut modifiers = Modifier::EMPTY;
    for (enabled, modifier) in [
        (style.bold, Modifier::BOLD),
        (style.dim, Modifier::DIM),
        (style.italic, Modifier::ITALIC),
        (style.underline, Modifier::UNDERLINE),
        (style.inverse, Modifier::REVERSED),
        (style.strike, Modifier::CROSSED_OUT),
    ] {
        if enabled {
            modifiers |= modifier;
        }
    }
    Style {
        fg: style.fg.map(color),
        bg: style.bg.map(color),
        modifiers,
    }
}

/// Publish after delivery only. This Arc contains the very same retained graph
/// used for painting; lookup never consults uncommitted layout state.
pub(super) fn commit(view: &mut AttachViewState, layout: &bmux_client::AttachLayoutState) {
    view.pane_zoom.committed = view.pane_zoom.prepared.clone();
    if view.pane_zoom.selection.as_ref().is_some_and(|selection| {
        view.pane_zoom
            .committed
            .get(&selection.projection.source.view)
            .is_none_or(|current| {
                current.capture_id != selection.projection.capture_id
                    || current.source != selection.projection.source
            })
    }) {
        view.pane_zoom.selection = None;
    }
    let Some(shared) = &view.pane_zoom.shared else {
        return;
    };
    let Some(identity) = view
        .observer
        .as_ref()
        .and_then(super::observer::AttachObserverPublisher::committed_prepared_identity)
    else {
        return;
    };
    let mut targets = BTreeMap::new();
    for surface in view.retained_compositor.surfaces().values().take(256) {
        let RetainedSurfacePayload::Content { content_id: pane } = surface.payload else {
            continue;
        };
        let Some(execution_id) = view.pane_zoom.execution_tokens.get(&pane).copied() else {
            continue;
        };
        targets.insert(
            surface.id,
            NativePaneZoomTarget {
                identity,
                pane_id: pane,
                view_id: surface.id,
                execution_id,
                source_revision: view
                    .pane_buffers
                    .get(&pane)
                    .map_or(0, |buffer| buffer.terminal_grid.grid().content_revision()),
                max_scale_millis: 8000,
                scale_millis: view
                    .pane_zoom
                    .requested
                    .get(&surface.id)
                    .map_or(1000, |(_, scale)| *scale),
            },
        );
    }
    if let Ok(mut shared) = shared.lock() {
        let max_scale_millis = shared.max_scale_millis;
        let progress_revision = shared.progress_revision.saturating_add(1);
        for target in targets.values_mut() {
            target.max_scale_millis = max_scale_millis.unwrap_or(8000);
        }
        *shared = CommittedZoom {
            max_scale_millis,
            progress_revision,
            identity: Some(identity),
            graph: view.retained_compositor.clone(),
            targets,
            focused: layout
                .scene
                .surfaces
                .iter()
                .find(|surface| {
                    surface.visible
                        && surface.cursor_owner
                        && surface.pane_id == Some(layout.focused_pane_id)
                })
                .map(|surface| surface.id),
        };
    }
}

pub(super) fn execution_at(
    view: &AttachViewState,
    point: [f64; 2],
    pane: Uuid,
) -> Option<ContentPosition> {
    let (content, local) = view.retained_compositor.content_cell_at(point)?;
    if content != pane
        || !local
            .iter()
            .all(|v| v.is_finite() && *v >= 0.0 && *v < 65536.0)
    {
        return None;
    }
    let projection = view
        .pane_zoom
        .committed
        .iter()
        .find_map(|(id, projection)| {
            if projection.source.pane != pane {
                return None;
            }
            let clips = view.retained_compositor.visible_content_clips(*id, 256)?;
            clips
                .iter()
                .any(|rect| {
                    point[0] >= f64::from(rect.x)
                        && point[1] >= f64::from(rect.y)
                        && point[0] < f64::from(rect.x) + f64::from(rect.w)
                        && point[1] < f64::from(rect.y) + f64::from(rect.h)
                })
                .then_some(projection)
        })?;
    let source = PaneZoomSource {
        execution: *view.pane_zoom.execution_tokens.get(&pane)?,
        ..projection.source
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // Checked finite local cell bounds above.
    let position = ContentPosition {
        column: local[0].floor() as usize,
        row: local[1].floor() as usize,
    };
    projection
        .execution_at(
            source,
            view.pane_buffers.get(&pane)?.terminal_grid.grid(),
            position,
        )
        .ok()
}

pub(super) async fn set_target(
    view: &mut AttachViewState,
    client: &mut bmux_client::StreamingBmuxClient,
    target: NativePaneZoomTarget,
    scale: u16,
) -> anyhow::Result<bool> {
    if !(125..=8000).contains(&scale) {
        return Ok(false);
    }
    if !view.pane_zoom.requested.contains_key(&target.view_id)
        && view.pane_zoom.requested.len() >= 32
    {
        return Ok(false);
    }
    let current = view
        .pane_zoom
        .shared
        .as_ref()
        .and_then(|shared| shared.lock().ok()?.targets.get(&target.view_id).copied());
    if scale > current.map_or(8000, |current| current.max_scale_millis) {
        anyhow::bail!("pane zoom exceeds effective raster cap");
    }
    if current != Some(target)
        || view.pane_zoom.execution_tokens.get(&target.pane_id) != Some(&target.execution_id)
    {
        return Ok(false);
    }
    let Some(buffer) = view.pane_buffers.get(&target.pane_id) else {
        return Ok(false);
    };
    if buffer.terminal_grid.grid().content_revision() != target.source_revision {
        return Ok(false);
    }
    let surface = view
        .retained_compositor
        .surfaces()
        .get(&target.view_id)
        .ok_or_else(|| anyhow::anyhow!("zoom surface unavailable"))?;
    let rect = surface.content_rect.unwrap_or(surface.rect);
    let columns = (usize::from(rect.w) * 1000 / usize::from(scale)).max(1);
    let rows = (usize::from(rect.h) * 1000 / usize::from(scale)).max(1);
    anyhow::ensure!(
        columns <= 1024 && rows <= 512 && columns.saturating_mul(rows) <= 262_144,
        "pane zoom exceeds projection geometry budget"
    );
    if buffer.terminal_grid.grid().mode() == GridMode::Alternate {
        if !view.can_write || view.observation_only {
            return Ok(false);
        }
        let token = *view
            .pane_zoom
            .execution_tokens
            .get(&target.pane_id)
            .ok_or_else(|| anyhow::anyhow!("alternate zoom execution identity unavailable"))?;
        let surface = view
            .retained_compositor
            .surfaces()
            .get(&target.view_id)
            .ok_or_else(|| anyhow::anyhow!("alternate zoom surface unavailable"))?;
        let rect = surface.content_rect.unwrap_or(surface.rect);
        let columns = (u32::from(rect.w) * 1000 / u32::from(scale)).clamp(1, 1000);
        let rows = (u32::from(rect.h) * 1000 / u32::from(scale)).clamp(1, 1000);
        if columns * rows > 262_144 || rows > 512 {
            return Ok(false);
        }
        pane_runtime_view_size_v1::client::resize_alternate(
            client,
            TerminalExecutionRef {
                session_id: view.attached_id,
                pane_id: target.pane_id,
                execution_token: token,
            },
            buffer.terminal_grid.grid().revision(),
            u16::try_from(columns)?,
            u16::try_from(rows)?,
        )
        .await?
        .map_err(|_| anyhow::anyhow!("alternate zoom source changed"))?;
        super::runtime::hydrate_attach_structured_grid_snapshots(
            client,
            view,
            vec![target.pane_id],
        )
        .await?;
    }
    if let Some(buffer) = view.pane_buffers.get_mut(&target.pane_id) {
        buffer.local_zoom.preserve_execution_geometry = true;
    }
    view.pane_zoom.requested.insert(
        target.view_id,
        (
            PaneZoomSource {
                view: target.view_id,
                pane: target.pane_id,
                execution: target.execution_id,
            },
            scale,
        ),
    );
    view.dirty
        .mark_full_frame(super::state::AttachDirtySource::PluginCommand);
    Ok(true)
}

/// Settle dequeue independently of presentation: rejected requests produce no frame.
pub(super) fn settle_dequeue(view: &AttachViewState) {
    if let Some(shared) = &view.pane_zoom.shared
        && let Ok(mut shared) = shared.lock()
    {
        shared.progress_revision = shared.progress_revision.saturating_add(1);
    }
}
