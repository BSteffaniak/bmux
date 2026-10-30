//! Local cell grids projected through caller-supplied retained visibility.
//!
//! These are output fragments, not a scene or interaction authority. The producer
//! supplies the final winning clips from its compositor, including all occlusion.
use std::sync::Arc;

use crate::buffer::Buffer;
use crate::frame::Cursor;
use crate::geometry::{Point, Rect};
use crate::image::{ImagePayload, ImagePlacement};

/// Checked uniform local-cell to root-cell transform. Scale is in thousandths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridTransform {
    origin: Point,
    scale_millis: u16,
}

impl GridTransform {
    /// Admit scales from 1/8 through 8. Origins are root presentation cells.
    #[must_use]
    pub const fn new(origin: Point, scale_millis: u16) -> Option<Self> {
        if scale_millis < 125 || scale_millis > 8000 {
            return None;
        }
        Some(Self {
            origin,
            scale_millis,
        })
    }

    #[must_use]
    pub const fn origin(self) -> Point {
        self.origin
    }

    #[must_use]
    pub fn scale(self) -> f64 {
        f64::from(self.scale_millis) / 1000.0
    }

    /// Forward mapping in root-cell units; no integer rounding before clipping.
    #[must_use]
    pub fn project(self, local: [f64; 2]) -> [f64; 2] {
        [
            local[0].mul_add(self.scale(), f64::from(self.origin.x)),
            local[1].mul_add(self.scale(), f64::from(self.origin.y)),
        ]
    }

    /// Inverse mapping for the compositor's fractional root-cell pointer position.
    /// Bounds and winning visibility must be checked by that compositor.
    #[must_use]
    pub fn unproject(self, root: [f64; 2]) -> Option<[f64; 2]> {
        if !root.iter().all(|value| value.is_finite()) {
            return None;
        }
        Some([
            (root[0] - f64::from(self.origin.x)) / self.scale(),
            (root[1] - f64::from(self.origin.y)) / self.scale(),
        ])
    }
}

/// Sparse original paint in root-cell coordinates. Only this buffer's rectangle
/// is replaced; absent background preserves lower background/image pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedGridOverlay {
    buffer: Arc<Buffer>,
    clip: Rect,
    paint_background: bool,
}
impl PreparedGridOverlay {
    #[must_use]
    pub fn new(buffer: Arc<Buffer>, clip: Rect, paint_background: bool) -> Option<Self> {
        if buffer.area().is_empty()
            || buffer.cells().len() > 1_000_000
            || clip.is_empty()
            || clip.intersection(buffer.area()) != clip
        {
            return None;
        }
        Some(Self {
            buffer,
            clip,
            paint_background,
        })
    }
    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }
    #[must_use]
    pub const fn clip(&self) -> Rect {
        self.clip
    }
    #[must_use]
    pub const fn paint_background(&self) -> bool {
        self.paint_background
    }
}

/// A local visual grid with compositor-winning disjoint root clips.
///
/// Opaque higher paint is excluded by clips; transparent paint is carried as original
/// ordered sparse overlays. No root-buffer repaint may reconstruct it.
/// Interaction/logical-source metadata remains owned by the enclosing snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedGridFragment {
    id: String,
    buffer: Arc<Buffer>,
    transform: GridTransform,
    visible_clips: Vec<Rect>,
    cursor: Option<Cursor>,
    images: Vec<Arc<ImagePlacement>>,
    overlays: Vec<PreparedGridOverlay>,
}

impl PreparedGridFragment {
    /// Validate bounded identity and local geometry before admission to a frame.
    /// Aggregate allocation and clip work are checked by the enclosing frame.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        buffer: Arc<Buffer>,
        transform: GridTransform,
        visible_clips: Vec<Rect>,
    ) -> Option<Self> {
        let id = id.into();
        let area = buffer.area();
        if id.is_empty()
            || id.len() > 1024
            || area.x != 0
            || area.y != 0
            || area.is_empty()
            || visible_clips.len() > 4096
            || visible_clips.iter().any(|clip| clip.is_empty())
        {
            return None;
        }
        Some(Self {
            id,
            buffer,
            transform,
            visible_clips,
            cursor: None,
            images: Vec::new(),
            overlays: Vec::new(),
        })
    }

    /// Cursor is local to this grid and shares its transform and winning clips.
    #[must_use]
    pub fn with_cursor(mut self, cursor: Cursor) -> Option<Self> {
        if !self.buffer.area().contains(cursor.position) {
            return None;
        }
        self.cursor = Some(cursor);
        Some(self)
    }

    /// Install a complete local image snapshot (not persistent lifecycle deltas).
    /// Local image destination/clip use this grid's cells; original allocation
    /// and fit are preserved for correct cropping. Payloads are shared, bounded
    /// here and charged again to the enclosing presentation's aggregate budget.
    #[must_use]
    pub fn with_images(mut self, images: Vec<Arc<ImagePlacement>>) -> Option<Self> {
        if images.len() > 4096 {
            return None;
        }
        let area = self.buffer.area();
        let mut bytes = 0_usize;
        for image in &images {
            bytes = bytes
                .checked_add(image.key.as_str().len())?
                .checked_add(image_payload_bytes(&image.payload))?;
            if bytes > 32 * 1024 * 1024
                || image.key.as_str().len() > 1024
                || image.destination.is_empty()
                || image.destination.intersection(area) != image.destination
                || image.clip.intersection(area) != image.clip
                || image
                    .allocation
                    .is_some_and(|allocation| !allocation.covers(image.destination))
            {
                return None;
            }
        }
        self.images = images;
        Some(self)
    }

    /// Install sparse root paint, replayed after this grid's images and cursor.
    /// Presentation admission charges all buffers and clip intersections.
    #[must_use]
    pub fn with_overlays(mut self, overlays: Vec<PreparedGridOverlay>) -> Option<Self> {
        if overlays.len() > 4096 {
            return None;
        }
        let cells = overlays.iter().try_fold(0_usize, |n, overlay| {
            n.checked_add(overlay.buffer.cells().len())
                .filter(|n| *n <= 1_000_000)
        })?;
        let bytes = overlays
            .iter()
            .flat_map(|overlay| overlay.buffer.cells())
            .try_fold(0_usize, |n, cell| n.checked_add(cell.symbol.len()))?;
        if cells > 1_000_000 || bytes > 16 * 1024 * 1024 {
            return None;
        }
        self.overlays = overlays;
        Some(self)
    }
    #[must_use]
    pub fn overlays(&self) -> &[PreparedGridOverlay] {
        &self.overlays
    }

    #[must_use]
    pub fn images(&self) -> &[Arc<ImagePlacement>] {
        &self.images
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }
    #[must_use]
    pub const fn transform(&self) -> GridTransform {
        self.transform
    }
    #[must_use]
    pub fn visible_clips(&self) -> &[Rect] {
        &self.visible_clips
    }
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }
}

pub(crate) const fn image_payload_bytes(payload: &ImagePayload) -> usize {
    match payload {
        ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.len(),
    }
}

/// This backend cannot consume local grids without losing geometry/metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedGridFragments;
impl std::fmt::Display for UnsupportedGridFragments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("backend does not support retained local-grid fragments")
    }
}
impl std::error::Error for UnsupportedGridFragments {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_image_snapshot_checks_geometry_and_payload_budget() {
        use crate::image::{
            ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
        };
        let fragment = PreparedGridFragment::new(
            "grid",
            Arc::new(Buffer::empty(Rect::new(0, 0, 2, 2))),
            GridTransform::new(Point::new(0, 0), 875).unwrap(),
            vec![Rect::new(0, 0, 1, 1)],
        )
        .unwrap();
        let mut image = ImagePlacement {
            key: ImageKey::new("image"),
            payload: ImagePayload::Pixels {
                bytes: vec![255; 4],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8,
            },
            destination: Rect::new(0, 0, 2, 2),
            allocation: None,
            clip: Rect::new(0, 0, 2, 2),
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Frame,
        };
        let with_image = fragment
            .clone()
            .with_images(vec![Arc::new(image.clone())])
            .unwrap();
        assert_eq!(with_image.images().len(), 1);
        let mut prepared = crate::presentation::PresentationState::new(Rect::new(0, 0, 2, 2))
            .prepare(crate::damage::Damage::Full, |_| {})
            .unwrap();
        let limits = crate::presentation::PresentationLimits {
            max_cells: 16,
            max_entries: 16,
            max_bytes: 1024,
        };
        prepared
            .set_grid_fragments(vec![with_image], limits)
            .unwrap();
        assert!(
            prepared
                .validate_limits(crate::presentation::PresentationLimits {
                    max_bytes: 1,
                    ..limits
                })
                .is_err()
        );
        image.destination = Rect::new(1, 1, 2, 2);
        assert!(fragment.with_images(vec![Arc::new(image)]).is_none());
    }

    #[test]
    fn fractional_inverse_uses_same_transform() {
        let transform = GridTransform::new(Point::new(3, 4), 500).unwrap();
        assert_eq!(transform.project([1.5, 2.5]), [3.75, 5.25]);
        assert_eq!(transform.unproject([3.75, 5.25]), Some([1.5, 2.5]));
        assert_eq!(transform.unproject([f64::NAN, 0.0]), None);
        assert!(GridTransform::new(Point::new(0, 0), 0).is_none());
    }
    #[test]
    fn grid_is_local_and_cursor_is_bounded() {
        let transform = GridTransform::new(Point::new(0, 0), 1000).unwrap();
        assert!(
            PreparedGridFragment::new(
                "x",
                Arc::new(Buffer::empty(Rect::new(1, 0, 2, 2))),
                transform,
                vec![]
            )
            .is_none()
        );
        let fragment = PreparedGridFragment::new(
            "x",
            Arc::new(Buffer::empty(Rect::new(0, 0, 2, 2))),
            transform,
            vec![],
        )
        .unwrap();
        assert!(
            fragment
                .with_cursor(Cursor::visible(Point::new(2, 0)))
                .is_none()
        );
    }
}
