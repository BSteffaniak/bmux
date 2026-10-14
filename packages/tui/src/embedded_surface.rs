//! Bounded inert raster embedding in the ordinary component/paint lifecycle.
//!
//! These snapshots are derived output, not a second scene authority. The caller
//! supplies retained native cells and reconciled images, and publishes the exact
//! layout used for painting only after successful presentation. No input routing,
//! focus policy, transport, or application state is owned here.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::buffer::Buffer;
use crate::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use crate::event::{Event, EventOutcome};
use crate::geometry::Rect;
use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat};
use crate::image_scene::ImageScene;
use crate::paint::{LocalRect, PaintCx};
use crate::style::Style;

/// Caller-owned authority and version of an immutable raster projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceIdentity {
    pub owner: String,
    pub generation: u64,
    pub revision: u64,
}

/// Limits checked before accepting a retained projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceLimits {
    pub max_cells: usize,
    pub max_bytes: usize,
}

/// Malformed raster data or an exceeded resource budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    Invalid,
    Limit,
}

/// Centered placement of a fixed raster, clipped rather than reflowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfacePlacement {
    pub source_x: u16,
    pub source_y: u16,
    pub destination_x: u16,
    pub destination_y: u16,
    pub width: u16,
    pub height: u16,
}

impl SurfacePlacement {
    /// Center a source extent inside a viewport, clipping larger sources centrally.
    #[must_use]
    pub const fn centered(source_width: u16, source_height: u16, width: u16, height: u16) -> Self {
        Self {
            source_x: source_width.saturating_sub(width) / 2,
            source_y: source_height.saturating_sub(height) / 2,
            destination_x: width.saturating_sub(source_width) / 2,
            destination_y: height.saturating_sub(source_height) / 2,
            width: if source_width < width {
                source_width
            } else {
                width
            },
            height: if source_height < height {
                source_height
            } else {
                height
            },
        }
    }

    /// Map only visible local viewport cells to source cells.
    #[must_use]
    pub fn input_point(self, x: u16, y: u16) -> Option<(u16, u16)> {
        let x = x.checked_sub(self.destination_x)?;
        let y = y.checked_sub(self.destination_y)?;
        (x < self.width && y < self.height).then(|| (x + self.source_x, y + self.source_y))
    }
}

/// Source coordinates tagged with the snapshot the caller displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceInputPoint {
    pub identity: SurfaceIdentity,
    pub x: u16,
    pub y: u16,
}

/// Validated immutable output sharing native storage with its producer.
#[derive(Clone, Debug)]
pub struct RetainedSurface {
    identity: SurfaceIdentity,
    cells: Arc<Buffer>,
    images: Arc<ImageScene>,
}

impl RetainedSurface {
    /// Retain an origin-zero raster and its complete reconciled image scene.
    ///
    /// PNG decode is bounded and performed at ingestion, never during painting.
    /// The buffers remain immutable behind shared handles; cloning a snapshot
    /// does not clone cell strings or image payloads.
    ///
    /// # Errors
    /// Returns [`SurfaceError::Invalid`] for malformed identity, cells, images,
    /// or PNG contents, and [`SurfaceError::Limit`] for resource admission failure.
    pub fn new(
        identity: SurfaceIdentity,
        cells: Arc<Buffer>,
        images: Arc<ImageScene>,
        limits: SurfaceLimits,
    ) -> Result<Self, SurfaceError> {
        if identity.owner.is_empty()
            || identity.owner.len() > 128
            || identity.owner.chars().any(char::is_control)
        {
            return Err(SurfaceError::Invalid);
        }
        validate_cells(&cells, limits)?;
        validate_images(&images, cells.area(), limits)?;
        let cell_bytes = cells.cells().iter().try_fold(0_usize, |total, cell| {
            total
                .checked_add(cell.symbol.len())
                .ok_or(SurfaceError::Limit)
        })?;
        let bytes = images.placements().try_fold(cell_bytes, |total, image| {
            let payload = match &image.payload {
                ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.len(),
            };
            total
                .checked_add(payload)
                .and_then(|value| value.checked_add(image.key.as_str().len()))
                .ok_or(SurfaceError::Limit)
        })?;
        if bytes > limits.max_bytes {
            return Err(SurfaceError::Limit);
        }
        Ok(Self {
            identity,
            cells,
            images,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> &SurfaceIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn buffer(&self) -> &Arc<Buffer> {
        &self.cells
    }

    #[must_use]
    pub const fn images(&self) -> &Arc<ImageScene> {
        &self.images
    }

    /// Derive fixed-canvas placement for an already resolved viewport.
    #[must_use]
    pub fn placement(&self, width: u16, height: u16) -> SurfacePlacement {
        let area = self.cells.area();
        SurfacePlacement::centered(area.width, area.height, width, height)
    }

    /// Paint with the same scoped transform used by every other component.
    /// This does not publish interaction geometry or commit presentation.
    pub fn paint_viewport(
        &self,
        cx: &mut PaintCx<'_, '_>,
        width: u16,
        height: u16,
    ) -> SurfacePlacement {
        let placement = self.placement(width, height);
        let area = self.cells.area();
        // The viewport scope also bounds images when an ancestor has a larger clip.
        cx.with_child(0, 0, LocalRect::new(0, 0, width, height), |viewport| {
            viewport.declare_metadata_availability(
                LocalRect::new(0, 0, width, height),
                false,
                false,
            );
            viewport.fill(LocalRect::new(0, 0, width, height), " ", Style::new());
            let dx = i32::from(placement.destination_x) - i32::from(placement.source_x);
            let dy = i64::from(placement.destination_y) - i64::from(placement.source_y);
            viewport.with_child(
                dx,
                dy,
                LocalRect::new(0, 0, area.width, area.height),
                |child| {
                    for (index, cell) in self.cells.cells().iter().enumerate() {
                        if cell.is_wide_continuation() {
                            continue;
                        }
                        let x = u16::try_from(index % usize::from(area.width)).unwrap_or(0);
                        let y = u16::try_from(index / usize::from(area.width)).unwrap_or(0);
                        // PaintCx owns atomic wide-glyph clipping.
                        child.set_cell(i32::from(x), i64::from(y), &cell.symbol, cell.style);
                    }
                    for image in self.images.placements() {
                        let mut image = image.clone();
                        // Length-prefix the owner so namespace boundaries cannot collide.
                        image.key = ImageKey::new(format!(
                            "{}:{}:{}:{}",
                            self.identity.owner.len(),
                            self.identity.owner,
                            self.identity.generation,
                            image.key.as_str(),
                        ));
                        // The snapshot contains the complete reconciled scene. Its host
                        // contribution is frame-scoped so removing the embedding removes it.
                        image.lifecycle = ImageLifecycle::Frame;
                        child.push_image(ImageContribution::Present(image));
                    }
                },
            );
        });
        placement
    }
}

/// A borrowed inert raster component; application input transport stays outside.
pub struct EmbeddedSurface<'a> {
    surface: &'a RetainedSurface,
}

impl<'a> EmbeddedSurface<'a> {
    #[must_use]
    pub const fn new(surface: &'a RetainedSurface) -> Self {
        Self { surface }
    }

    fn id(&self) -> LayoutId {
        let identity = self.surface.identity();
        // Version-tagging rejects layout from a different retained snapshot.
        LayoutId::new(format!(
            "embedded/{}:{}/{}/{}",
            identity.owner.len(),
            identity.owner,
            identity.generation,
            identity.revision
        ))
    }

    /// The caller must pass its last successfully displayed layout, not a pending resize.
    #[must_use]
    pub fn placement(&self, committed: &LayoutNode) -> Option<SurfacePlacement> {
        if committed.id != self.id() {
            return None;
        }
        Some(self.surface.placement(
            u16::try_from(committed.size.width).ok()?,
            u16::try_from(committed.size.height).ok()?,
        ))
    }

    /// Translate local coordinates without creating a parallel hit/focus map.
    #[must_use]
    pub fn input_point(&self, committed: &LayoutNode, x: u16, y: u16) -> Option<SurfaceInputPoint> {
        let (x, y) = self.placement(committed)?.input_point(x, y)?;
        Some(SurfaceInputPoint {
            identity: self.surface.identity.clone(),
            x,
            y,
        })
    }
}

impl Component for EmbeddedSurface<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        let area = self.surface.cells.area();
        LayoutNode::leaf(
            self.id(),
            constraints.constrain(LogicalSize::new(
                u64::from(area.width),
                u64::from(area.height),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if self.placement(layout).is_none() {
            return;
        }
        let (Ok(width), Ok(height)) = (
            u16::try_from(layout.size.width),
            u16::try_from(layout.size.height),
        ) else {
            return;
        };
        self.surface.paint_viewport(cx, width, height);
    }

    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }

    fn revision(&self) -> ComponentRevision {
        let area = self.surface.cells.area();
        // Match native components' deterministic per-process revision hashing.
        // Equal dimensions/revisions from a replacement owner or generation must
        // invalidate both layout retention and paint retention.
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.surface.identity.owner.hash(&mut layout);
        self.surface.identity.generation.hash(&mut layout);
        area.width.hash(&mut layout);
        area.height.hash(&mut layout);
        let layout_revision = layout.finish();
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        layout_revision.hash(&mut paint);
        self.surface.identity.revision.hash(&mut paint);
        ComponentRevision::new(layout_revision, paint.finish())
    }
}

fn validate_cells(buffer: &Buffer, limits: SurfaceLimits) -> Result<(), SurfaceError> {
    let area = buffer.area();
    let count = usize::from(area.width) * usize::from(area.height);
    if area.x != 0
        || area.y != 0
        || count == 0
        || count > limits.max_cells
        || buffer.cells().len() != count
    {
        return Err(SurfaceError::Limit);
    }
    for row in buffer.cells().chunks_exact(usize::from(area.width)) {
        for (index, cell) in row.iter().enumerate() {
            if cell.symbol.len() > 64 || cell.symbol.chars().any(char::is_control) {
                return Err(SurfaceError::Invalid);
            }
            if cell.is_wide_continuation() {
                if cell.width() != 0
                    || !cell.symbol.is_empty()
                    || index == 0
                    || !row[index - 1].is_wide_leader()
                    || cell.style != row[index - 1].style
                {
                    return Err(SurfaceError::Invalid);
                }
            } else if cell.symbol.graphemes(true).count() != 1
                || cell.symbol.width() != usize::from(cell.width())
                || !(1..=2).contains(&cell.width())
                || (cell.width() == 2
                    && row
                        .get(index + 1)
                        .is_none_or(|next| !next.is_wide_continuation()))
            {
                return Err(SurfaceError::Invalid);
            }
        }
    }
    Ok(())
}

fn validate_images(
    scene: &ImageScene,
    area: Rect,
    limits: SurfaceLimits,
) -> Result<(), SurfaceError> {
    if scene.placements().len() > limits.max_cells {
        return Err(SurfaceError::Limit);
    }
    let mut decoded_total = 0_usize;
    let mut encoded_total = 0_usize;
    for placement in scene.placements() {
        let key = placement.key.as_str();
        let within = |rect: Rect| {
            !rect.is_empty()
                && u32::from(rect.x) + u32::from(rect.width) <= u32::from(area.width)
                && u32::from(rect.y) + u32::from(rect.height) <= u32::from(area.height)
        };
        if key.is_empty()
            || key.len() > 128
            || key.chars().any(char::is_control)
            || !within(placement.destination)
            || !within(placement.clip)
            || placement
                .allocation
                .is_some_and(|bounds| !bounds.covers(placement.destination))
        {
            return Err(SurfaceError::Invalid);
        }
        let (bytes, width, height, channels) = match &placement.payload {
            ImagePayload::Png {
                bytes,
                width,
                height,
            } => (bytes, *width, *height, 4_usize),
            ImagePayload::Pixels {
                bytes,
                width,
                height,
                format,
            } => (
                bytes,
                *width,
                *height,
                if *format == ImagePixelFormat::Rgba8 {
                    4
                } else {
                    3
                },
            ),
        };
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err(SurfaceError::Limit);
        }
        let decoded = usize::try_from(width)
            .map_err(|_| SurfaceError::Limit)?
            .checked_mul(usize::try_from(height).map_err(|_| SurfaceError::Limit)?)
            .and_then(|pixels| pixels.checked_mul(channels))
            .ok_or(SurfaceError::Limit)?;
        decoded_total = decoded_total
            .checked_add(decoded)
            .ok_or(SurfaceError::Limit)?;
        encoded_total = encoded_total
            .checked_add(bytes.len())
            .ok_or(SurfaceError::Limit)?;
        if decoded > 16 * 1024 * 1024
            || decoded_total > 32 * 1024 * 1024
            || encoded_total > limits.max_bytes
        {
            return Err(SurfaceError::Limit);
        }
        match &placement.payload {
            ImagePayload::Pixels { .. } if bytes.len() != decoded => {
                return Err(SurfaceError::Invalid);
            }
            ImagePayload::Png { .. } => {
                let mut reader = image::ImageReader::with_format(
                    std::io::Cursor::new(&bytes[..]),
                    image::ImageFormat::Png,
                );
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(width);
                limits.max_image_height = Some(height);
                limits.max_alloc = Some(32 * 1024 * 1024);
                reader.limits(limits);
                let decoded = reader.decode().map_err(|_| SurfaceError::Invalid)?;
                if decoded.width() != width || decoded.height() != height {
                    return Err(SurfaceError::Invalid);
                }
            }
            ImagePayload::Pixels { .. } => {}
        }
    }
    Ok(())
}
