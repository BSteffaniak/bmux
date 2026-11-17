//! Pane images as image placements of the attach frame.
//!
//! Each image a pane shows (its live images, or its scrollback window's) becomes placements of
//! the frame the client presents: decoded once into a shared payload, placed at its cells in the
//! pane's content, and split into the strips that stay visible around surfaces drawn over it.
//! The frame's image scene is then the one source of what images show: the presenter's image
//! compositor writes it for the host, and observers mirror it.

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
use std::collections::HashMap;

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
use bmux_attach_image_protocol::{AttachImageProtocol, AttachPaneImage, CompressionId};
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
use bmux_tui::geometry::Rect;
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
use bmux_tui::image::{ImagePayload, ImagePlacement};
use uuid::Uuid;

/// The prefix of every pane image's key: `pane:<surface id>:`, so a surface's pane images can be
/// told apart from other images in the scene.
pub(super) fn surface_key_prefix(surface: Uuid) -> String {
    format!("pane:{surface}:")
}

/// Most bytes of pane image payloads one frame places.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
const MAX_FRAME_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
/// Most pane image placements one frame holds.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
const MAX_FRAME_PLACEMENTS: usize = 4096;

/// Which pane image a cached payload belongs to: its id within its pane execution.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ImageIdentity {
    pane: Uuid,
    incarnation: Uuid,
    image: u64,
}

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
struct DecodedImage {
    protocol: AttachImageProtocol,
    compression: CompressionId,
    pixels: (u32, u32),
    /// The protocol bytes decoded: an image is only ever matched by its content.
    raw: Box<[u8]>,
    payload: ImagePayload,
}

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
impl DecodedImage {
    fn decodes(&self, image: &AttachPaneImage) -> bool {
        self.protocol == image.protocol
            && self.compression == image.compression
            && self.pixels == (image.pixel_width, image.pixel_height)
            && *self.raw == *image.raw_data
    }
}

/// Payloads of the pane images last placed, decoded once and shared by every frame that places
/// them. Only images placed by the latest frame are kept.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
#[derive(Default)]
pub struct PaneImagePayloads {
    decoded: HashMap<ImageIdentity, DecodedImage>,
}

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
impl PaneImagePayloads {
    /// The placements of every pane image the frame shows, in surface order.
    ///
    /// An image that cannot be decoded, or that would take the frame past its image budget, is
    /// left out (and logged); the frame shows the rest.
    pub(super) fn placements(
        &mut self,
        view: &super::state::AttachViewState,
        layout: &bmux_client::AttachLayoutState,
        area: Rect,
    ) -> Vec<ImagePlacement> {
        let mut decoded = HashMap::with_capacity(self.decoded.len());
        let mut placements = Vec::new();
        let mut bytes = 0_usize;
        for surface in &layout.scene.surfaces {
            let Some(pane) = surface.pane_id else {
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
            let incarnation = view
                .image_execution_incarnations
                .get(&pane)
                .copied()
                .unwrap_or(Uuid::nil());
            for image in view.presented_images(pane) {
                let identity = ImageIdentity {
                    pane,
                    incarnation,
                    image: image.id,
                };
                let payload = match self.payload(&mut decoded, identity, image) {
                    Ok(payload) => payload,
                    Err(error) => {
                        tracing::warn!(%pane, image = image.id, %error, "pane image not shown");
                        continue;
                    }
                };
                let destination = Rect::new(
                    content.x.saturating_add(image.position_col),
                    content.y.saturating_add(image.position_row),
                    image.cell_cols,
                    image.cell_rows,
                );
                let key = format!(
                    "{}{pane}:{incarnation}:{}",
                    surface_key_prefix(surface.id),
                    image.id
                );
                let strips = bmux_attach_pipeline::projection_images::visible_image_strips(
                    &key,
                    &payload,
                    destination,
                    content.intersection(area),
                    |x, y| {
                        view.retained_compositor
                            .point_visible_from(surface.id, x, y)
                    },
                );
                let strips = match strips {
                    Ok(strips) => strips,
                    Err(error) => {
                        tracing::warn!(%pane, image = image.id, %error, "pane image not shown");
                        continue;
                    }
                };
                let size = payload_bytes(&payload).saturating_mul(strips.len());
                if bytes.saturating_add(size) > MAX_FRAME_PAYLOAD_BYTES
                    || placements.len() + strips.len() > MAX_FRAME_PLACEMENTS
                {
                    tracing::warn!(%pane, image = image.id, "pane image budget exceeded");
                    continue;
                }
                bytes += size;
                placements.extend(strips);
            }
        }
        self.decoded = decoded;
        placements
    }

    /// The payload of `image`, decoded now unless the last frame placed it: moved into `next`.
    fn payload(
        &mut self,
        next: &mut HashMap<ImageIdentity, DecodedImage>,
        identity: ImageIdentity,
        image: &AttachPaneImage,
    ) -> Result<ImagePayload, String> {
        if let Some(entry) = next.get(&identity).filter(|entry| entry.decodes(image)) {
            return Ok(entry.payload.clone());
        }
        let entry = match self.decoded.remove(&identity) {
            Some(entry) if entry.decodes(image) => entry,
            _ => {
                let payload = image_payload(image)?;
                // Shared with observers as is: large raw pixels travel as PNG.
                let payload = bmux_attach_pipeline::projection_images::compact_payload(&payload)?
                    .into_owned();
                DecodedImage {
                    protocol: image.protocol,
                    compression: image.compression,
                    pixels: (image.pixel_width, image.pixel_height),
                    raw: image.raw_data.clone().into_boxed_slice(),
                    payload,
                }
            }
        };
        let payload = entry.payload.clone();
        next.insert(identity, entry);
        Ok(payload)
    }
}

#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
fn payload_bytes(payload: &ImagePayload) -> usize {
    match payload {
        ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.len(),
    }
}

/// Decode a pane image's protocol bytes into a payload any host or observer can show, within
/// the attach client's budgets.
///
/// # Errors
/// Returns why the image cannot be decoded: over budget, malformed, or of a protocol this build
/// does not decode.
#[cfg(any(
    feature = "image-sixel",
    feature = "image-kitty",
    feature = "image-iterm2"
))]
pub(super) fn image_payload(image: &AttachPaneImage) -> Result<ImagePayload, String> {
    if image.raw_data.len() > 16 * 1024 * 1024 {
        return Err("encoded image budget exceeded".into());
    }
    if image.protocol == AttachImageProtocol::Sixel
        && u64::from(image.pixel_width) * u64::from(image.pixel_height)
            > bmux_attach_pipeline::projection_images::MAX_IMAGE_PIXELS
    {
        return Err("sixel pixel budget exceeded".into());
    }
    bmux_image::tui::pane_image_payload(&bmux_image::PaneImage::from(image)).map_err(Into::into)
}

#[cfg(all(
    test,
    any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    )
))]
mod tests {
    use super::{ImageIdentity, PaneImagePayloads};
    use bmux_attach_image_protocol::{AttachImageProtocol, AttachPaneImage, CompressionId};
    use std::collections::HashMap;
    use uuid::Uuid;

    const fn image(id: u64, raw_data: Vec<u8>) -> AttachPaneImage {
        AttachPaneImage {
            id,
            protocol: AttachImageProtocol::KittyGraphics,
            raw_data,
            compression: CompressionId::None,
            position_row: 0,
            position_col: 0,
            cell_rows: 1,
            cell_cols: 1,
            pixel_width: 1,
            pixel_height: 1,
        }
    }

    /// A payload is decoded once and shared while frames keep placing its image, and a reused id
    /// with other bytes is decoded again.
    #[test]
    fn payloads_are_decoded_once_and_kept_while_placed() {
        let identity = |image| ImageIdentity {
            pane: Uuid::nil(),
            incarnation: Uuid::nil(),
            image,
        };
        let mut payloads = PaneImagePayloads::default();
        let first = image(1, vec![1, 2, 3]);
        let mut next = HashMap::new();
        let decoded = payloads.payload(&mut next, identity(1), &first).unwrap();
        payloads.decoded = next;
        let mut next = HashMap::new();
        let shared = payloads.payload(&mut next, identity(1), &first).unwrap();
        assert_eq!(shared, decoded);
        let (
            bmux_tui::image::ImagePayload::Png { bytes: a, .. },
            bmux_tui::image::ImagePayload::Png { bytes: b, .. },
        ) = (&decoded, &shared)
        else {
            panic!("Kitty images are PNG payloads");
        };
        assert!(
            std::sync::Arc::ptr_eq(a, b),
            "the payload is shared, not decoded again"
        );
        payloads.decoded = next;
        // The cached entry was decoded from other bytes of the same length under the same id.
        let reused = image(1, vec![9, 9, 9]);
        let mut next = HashMap::new();
        let redecoded = payloads.payload(&mut next, identity(1), &reused).unwrap();
        assert_ne!(redecoded, decoded);
    }
}
