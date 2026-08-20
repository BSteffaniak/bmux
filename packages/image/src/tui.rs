//! Adapter from protocol-neutral TUI image scenes into BMUX image compositing.

use std::collections::BTreeMap;
use std::io::Write;

use bmux_tui::image::{ImageKey, ImagePayload as TuiImagePayload, ImagePixelFormat};
use bmux_tui::image_scene::{ImageScene, ImageSceneDelta};

use crate::compositor::{KittyHostState, PaneRect, render_pane_images};
use crate::config::{ImageConfig, ImageDecodeMode};
use crate::host_caps::HostImageCapabilities;
use crate::model::{
    ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, PaneImage,
    PixelBuffer, PixelFormat,
};
use crate::registry::ImageRegistry;

/// Failure while adapting a TUI image scene into BMUX's image pipeline.
#[derive(Debug)]
pub enum TuiImageError {
    /// An image payload could not be decoded or failed validation.
    InvalidPayload {
        /// Stable key of the rejected image.
        key: ImageKey,
        /// Human-readable validation failure.
        reason: String,
    },
    /// Image output failed.
    Io(std::io::Error),
}

impl std::fmt::Display for TuiImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPayload { key, reason } => {
                write!(formatter, "invalid TUI image '{}': {reason}", key.as_str())
            }
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TuiImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPayload { .. } => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for TuiImageError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// BMUX-owned adapter that assigns image IDs, validates payloads, clips
/// placements, and delegates host protocol selection to the compositor.
#[derive(Default, Clone)]
pub struct TuiImageCompositor {
    next_id: u64,
    ids: BTreeMap<ImageKey, u64>,
    payloads: BTreeMap<ImageKey, TuiImagePayload>,
    visible_ids: std::collections::BTreeSet<u64>,
    registry: ImageRegistry,
    pending_removals: Vec<u64>,
    kitty_state: KittyHostState,
}

impl TuiImageCompositor {
    /// Create an empty compositor adapter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply lifecycle removals produced by the TUI frame reconciler.
    pub fn apply_delta(&mut self, delta: &ImageSceneDelta) {
        for key in &delta.removed {
            if let Some(id) = self.ids.remove(key) {
                self.pending_removals.push(id);
            }
            self.payloads.remove(key);
        }
    }

    /// Render the current TUI image scene as a terminal overlay.
    ///
    /// Host capability detection and protocol selection remain entirely inside
    /// BMUX. Unsupported hosts produce no image escape sequences.
    ///
    /// # Errors
    ///
    /// Returns [`TuiImageError::InvalidPayload`] for malformed or oversized
    /// payloads and [`TuiImageError::Io`] when the output writer fails.
    pub fn render(
        &mut self,
        out: &mut impl Write,
        scene: &ImageScene,
        terminal: PaneRect,
        host_caps: &HostImageCapabilities,
        config: &ImageConfig,
    ) -> Result<(), TuiImageError> {
        if !config.enabled || !host_caps.any_supported() {
            return Ok(());
        }

        self.emit_pending_removals(out, host_caps)?;
        let mut images = Vec::with_capacity(scene.placements().len());
        let mut visible_ids = std::collections::BTreeSet::new();
        for placement in scene.placements() {
            let Some(visible) = clipped_destination(
                placement.destination,
                placement.clip,
                bmux_tui::geometry::Rect::new(terminal.x, terminal.y, terminal.w, terminal.h),
            ) else {
                continue;
            };
            let id = if self.payloads.get(&placement.key) == Some(&placement.payload) {
                *self.ids.entry(placement.key.clone()).or_insert_with(|| {
                    self.next_id = self.next_id.saturating_add(1);
                    self.next_id
                })
            } else {
                if let Some(previous_id) = self.ids.get(&placement.key).copied() {
                    self.pending_removals.push(previous_id);
                }
                self.next_id = self.next_id.saturating_add(1);
                self.ids.insert(placement.key.clone(), self.next_id);
                self.payloads
                    .insert(placement.key.clone(), placement.payload.clone());
                self.next_id
            };
            let mut payload = decode_payload(&placement.key, &placement.payload, config)?;
            let visible_pixel_size = crop_to_visible(&mut payload, placement.destination, visible);
            let position = ImagePosition {
                row: visible.y.saturating_sub(terminal.y),
                col: visible.x.saturating_sub(terminal.x),
            };
            visible_ids.insert(id);
            images.push(PaneImage {
                id,
                protocol: ImageProtocol::KittyGraphics,
                payload,
                position,
                cell_size: ImageCellSize {
                    rows: visible.height,
                    cols: visible.width,
                },
                pixel_size: visible_pixel_size,
            });
        }

        self.emit_pending_removals(out, host_caps)?;
        self.emit_hidden_removals(out, host_caps, &visible_ids)?;
        self.visible_ids = visible_ids;
        self.registry.replace_images(images);
        render_pane_images(
            out,
            self.registry.images(),
            terminal,
            host_caps,
            ImageDecodeMode::Server,
            &mut self.kitty_state,
        )?;
        Ok(())
    }

    /// Queue every active image for protocol-specific removal.
    pub fn clear(&mut self) {
        self.pending_removals.extend(self.ids.values().copied());
        self.ids.clear();
        self.payloads.clear();
        self.visible_ids.clear();
        self.registry.clear();
    }

    /// Emit queued removals and clear all compositor-owned host resources.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when protocol cleanup cannot be written.
    pub fn cleanup(
        &mut self,
        out: &mut impl Write,
        host_caps: &HostImageCapabilities,
    ) -> std::io::Result<()> {
        self.clear();
        self.emit_pending_removals(out, host_caps)
    }

    fn emit_hidden_removals(
        &mut self,
        out: &mut impl Write,
        host_caps: &HostImageCapabilities,
        visible_ids: &std::collections::BTreeSet<u64>,
    ) -> std::io::Result<()> {
        #[cfg(not(feature = "kitty"))]
        let _ = (&mut *out, host_caps, visible_ids);
        #[cfg(feature = "kitty")]
        if host_caps.preferred_protocol() == Some(ImageProtocol::KittyGraphics) {
            for id in self.visible_ids.difference(visible_ids) {
                if let Some(host_id) = self.kitty_state.transmitted.remove(id) {
                    out.write_all(b"\x1b_")?;
                    out.write_all(&crate::codec::kitty::encode_delete_image(host_id))?;
                    out.write_all(b"\x1b\\")?;
                }
            }
        }
        Ok(())
    }

    fn emit_pending_removals(
        &mut self,
        out: &mut impl Write,
        host_caps: &HostImageCapabilities,
    ) -> std::io::Result<()> {
        #[cfg(not(feature = "kitty"))]
        let _ = (&mut *out, host_caps);
        #[cfg(feature = "kitty")]
        if host_caps.preferred_protocol() == Some(ImageProtocol::KittyGraphics) {
            for id in self.pending_removals.drain(..) {
                if let Some(host_id) = self.kitty_state.transmitted.remove(&id) {
                    out.write_all(b"\x1b_")?;
                    out.write_all(&crate::codec::kitty::encode_delete_image(host_id))?;
                    out.write_all(b"\x1b\\")?;
                }
            }
            return Ok(());
        }
        self.pending_removals.clear();
        Ok(())
    }
}

fn clipped_destination(
    destination: bmux_tui::geometry::Rect,
    clip: bmux_tui::geometry::Rect,
    terminal: bmux_tui::geometry::Rect,
) -> Option<bmux_tui::geometry::Rect> {
    let visible = destination.intersection(clip).intersection(terminal);
    if visible.is_empty() {
        return None;
    }
    Some(visible)
}

fn crop_to_visible(
    payload: &mut ImagePayload,
    destination: bmux_tui::geometry::Rect,
    visible: bmux_tui::geometry::Rect,
) -> ImagePixelSize {
    let Some(pixels) = payload.pixels.as_mut() else {
        return ImagePixelSize {
            width: 0,
            height: 0,
        };
    };
    let source_x = proportional_pixels_floor(
        pixels.width,
        visible.x.saturating_sub(destination.x),
        destination.width,
    );
    let source_y = proportional_pixels_floor(
        pixels.height,
        visible.y.saturating_sub(destination.y),
        destination.height,
    );
    let source_end_x = proportional_pixels(
        pixels.width,
        visible.right().saturating_sub(destination.x),
        destination.width,
    );
    let source_end_y = proportional_pixels(
        pixels.height,
        visible.bottom().saturating_sub(destination.y),
        destination.height,
    );
    let cropped_width = source_end_x.saturating_sub(source_x);
    let cropped_height = source_end_y.saturating_sub(source_y);
    if cropped_width == pixels.width && cropped_height == pixels.height {
        return ImagePixelSize {
            width: pixels.width,
            height: pixels.height,
        };
    }
    let bytes_per_pixel = match pixels.format {
        PixelFormat::Rgb8 => 3,
        PixelFormat::Rgba8 => 4,
        PixelFormat::Png => {
            return ImagePixelSize {
                width: 0,
                height: 0,
            };
        }
    };
    let row_bytes = usize::try_from(pixels.width)
        .unwrap_or(usize::MAX)
        .saturating_mul(bytes_per_pixel);
    let cropped_row_bytes = usize::try_from(cropped_width)
        .unwrap_or(usize::MAX)
        .saturating_mul(bytes_per_pixel);
    let mut data = Vec::with_capacity(
        cropped_row_bytes.saturating_mul(usize::try_from(cropped_height).unwrap_or(usize::MAX)),
    );
    for row in pixels
        .data
        .chunks(row_bytes)
        .skip(usize::try_from(source_y).unwrap_or(usize::MAX))
        .take(usize::try_from(cropped_height).unwrap_or(usize::MAX))
    {
        let start = usize::try_from(source_x)
            .unwrap_or(usize::MAX)
            .saturating_mul(bytes_per_pixel)
            .min(row.len());
        let end = start.saturating_add(cropped_row_bytes).min(row.len());
        data.extend_from_slice(&row[start..end]);
    }
    pixels.width = cropped_width;
    pixels.height = cropped_height;
    pixels.data = data;
    ImagePixelSize {
        width: cropped_width,
        height: cropped_height,
    }
}

fn proportional_pixels_floor(pixels: u32, cells: u16, destination_cells: u16) -> u32 {
    if destination_cells == 0 {
        return 0;
    }
    pixels
        .saturating_mul(u32::from(cells))
        .checked_div(u32::from(destination_cells))
        .unwrap_or(0)
        .min(pixels)
}

fn proportional_pixels(pixels: u32, visible_cells: u16, destination_cells: u16) -> u32 {
    if destination_cells == 0 {
        return 0;
    }
    pixels
        .saturating_mul(u32::from(visible_cells))
        .div_ceil(u32::from(destination_cells))
        .min(pixels)
}

fn decode_payload(
    key: &ImageKey,
    payload: &TuiImagePayload,
    config: &ImageConfig,
) -> Result<ImagePayload, TuiImageError> {
    let (bytes, width, height, format) = match payload {
        TuiImagePayload::Pixels {
            bytes,
            width,
            height,
            format,
        } => {
            validate_pixels(key, bytes, *width, *height, *format, config.max_image_bytes)?;
            let format = match format {
                ImagePixelFormat::Rgb8 => PixelFormat::Rgb8,
                ImagePixelFormat::Rgba8 => PixelFormat::Rgba8,
            };
            (bytes.clone(), *width, *height, format)
        }
        TuiImagePayload::Png {
            bytes,
            width,
            height,
        } => {
            if bytes.len() > config.max_image_bytes {
                return Err(invalid(
                    key,
                    "encoded payload exceeds configured byte limit",
                ));
            }
            let decoder = image::ImageReader::with_format(
                std::io::Cursor::new(bytes),
                image::ImageFormat::Png,
            );
            let decoded = decoder
                .decode()
                .map_err(|error| invalid(key, format!("PNG decode failed: {error}")))?;
            if decoded.width() != *width || decoded.height() != *height {
                return Err(invalid(
                    key,
                    "declared dimensions do not match decoded PNG dimensions",
                ));
            }
            let rgba = decoded.into_rgba8().into_raw();
            if rgba.len() > config.max_image_bytes {
                return Err(invalid(key, "decoded pixels exceed configured byte limit"));
            }
            (rgba, *width, *height, PixelFormat::Rgba8)
        }
    };

    Ok(ImagePayload {
        raw: None,
        pixels: Some(PixelBuffer {
            width,
            height,
            format,
            data: bytes,
        }),
    })
}

fn validate_pixels(
    key: &ImageKey,
    bytes: &[u8],
    width: u32,
    height: u32,
    format: ImagePixelFormat,
    max_bytes: usize,
) -> Result<(), TuiImageError> {
    let bytes_per_pixel = match format {
        ImagePixelFormat::Rgb8 => 3_u64,
        ImagePixelFormat::Rgba8 => 4_u64,
    };
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| invalid(key, "pixel dimensions overflow"))?;
    if expected != bytes.len() {
        return Err(invalid(key, "pixel byte length does not match dimensions"));
    }
    if expected > max_bytes {
        return Err(invalid(key, "pixel payload exceeds configured byte limit"));
    }
    Ok(())
}

fn invalid(key: &ImageKey, reason: impl Into<String>) -> TuiImageError {
    TuiImageError::InvalidPayload {
        key: key.clone(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{TuiImageCompositor, crop_to_visible};
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    };
    use bmux_tui::image_scene::ImageScene;

    use crate::compositor::PaneRect;
    use crate::config::ImageConfig;
    use crate::host_caps::HostImageCapabilities;

    fn placement(key: &str, destination: Rect, clip: Rect, value: u8) -> ImagePlacement {
        ImagePlacement {
            key: ImageKey::new(key),
            payload: ImagePayload::Pixels {
                bytes: vec![value; 8 * 4 * 4],
                width: 8,
                height: 4,
                format: ImagePixelFormat::Rgba8,
            },
            destination,
            clip,
            lifecycle: ImageLifecycle::Frame,
        }
    }

    fn kitty_caps() -> HostImageCapabilities {
        HostImageCapabilities {
            kitty_graphics: true,
            ..HostImageCapabilities::default()
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn renders_placement_with_host_selected_protocol() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(3, 2, 4, 2),
            Rect::new(0, 0, 20, 10),
            1,
        ))]);
        let mut compositor = TuiImageCompositor::new();
        let mut output = Vec::new();

        compositor
            .render(
                &mut output,
                &scene,
                PaneRect {
                    x: 1,
                    y: 1,
                    w: 10,
                    h: 5,
                },
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\u{1b}[3;4H"));
        assert!(output.contains("a=t"));
        assert!(output.contains("a=p"));
        assert!(output.contains("c=4,r=2"));
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn clips_right_and_bottom_edges_before_transmission() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(2, 1, 4, 2),
            Rect::new(0, 0, 4, 2),
            1,
        ))]);
        let mut compositor = TuiImageCompositor::new();
        let mut output = Vec::new();

        compositor
            .render(
                &mut output,
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 5,
                },
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("s=4,v=2"));
        assert!(output.contains("c=2,r=1"));
    }

    #[test]
    fn left_and_top_crop_selects_matching_pixel_rows_and_columns() {
        let mut payload = crate::model::ImagePayload {
            raw: None,
            pixels: Some(crate::model::PixelBuffer {
                width: 4,
                height: 2,
                format: crate::model::PixelFormat::Rgba8,
                data: (0_u8..32).collect(),
            }),
        };

        let size = crop_to_visible(&mut payload, Rect::new(0, 0, 4, 2), Rect::new(2, 1, 2, 1));

        assert_eq!(size.width, 2);
        assert_eq!(size.height, 1);
        let pixels = payload.pixels.expect("cropped pixels");
        assert_eq!(pixels.data, (24_u8..32).collect::<Vec<_>>());
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn clips_left_and_top_edges_to_the_matching_source_pixels() {
        let mut scene = ImageScene::default();
        let bytes = (0_u8..32).collect::<Vec<_>>();
        scene.reconcile(&[ImageContribution::Present(ImagePlacement {
            key: ImageKey::new("diagram"),
            payload: ImagePayload::Pixels {
                bytes,
                width: 4,
                height: 2,
                format: ImagePixelFormat::Rgba8,
            },
            destination: Rect::new(0, 0, 4, 2),
            clip: Rect::new(2, 1, 2, 1),
            lifecycle: ImageLifecycle::Frame,
        })]);
        let mut compositor = TuiImageCompositor::new();
        let mut output = Vec::new();

        compositor
            .render(
                &mut output,
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 5,
                },
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\u{1b}[2;3H"));
        assert!(output.contains("s=2,v=1"));
        assert!(output.contains("c=2,r=1"));
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn vertical_reposition_reuses_transmission_and_updates_cursor() {
        let mut scene = ImageScene::default();
        let mut compositor = TuiImageCompositor::new();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(1, 3, 4, 2),
            Rect::new(0, 0, 10, 5),
            1,
        ))]);
        compositor
            .render(
                &mut Vec::new(),
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();

        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(1, 1, 4, 2),
            Rect::new(0, 0, 10, 5),
            1,
        ))]);
        let mut upward = Vec::new();
        compositor
            .render(
                &mut upward,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        let upward = String::from_utf8(upward).unwrap();
        assert!(upward.starts_with("\u{1b}[2;2H"));
        assert!(!upward.contains("a=t"));
        assert!(upward.contains("a=p"));

        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(1, 2, 4, 2),
            Rect::new(0, 0, 10, 5),
            1,
        ))]);
        let mut downward = Vec::new();
        compositor
            .render(
                &mut downward,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        assert!(
            String::from_utf8(downward)
                .unwrap()
                .starts_with("\u{1b}[3;2H")
        );
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn replacement_and_removal_reconcile_stable_keys() {
        let mut scene = ImageScene::default();
        let mut compositor = TuiImageCompositor::new();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };
        let first = scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(0, 0, 4, 2),
            Rect::new(0, 0, 10, 5),
            1,
        ))]);
        compositor.apply_delta(&first);
        compositor
            .render(
                &mut Vec::new(),
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();

        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(0, 0, 4, 2),
            Rect::new(0, 0, 10, 5),
            2,
        ))]);
        let mut replaced = Vec::new();
        compositor
            .render(
                &mut replaced,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        let replaced = String::from_utf8(replaced).unwrap();
        assert!(replaced.contains("a=d"));
        assert!(replaced.contains("a=t"));

        let removed = scene.reconcile(&[]);
        compositor.apply_delta(&removed);
        let mut stale_removed = Vec::new();
        compositor
            .render(
                &mut stale_removed,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        assert!(String::from_utf8(stale_removed).unwrap().contains("a=d"));
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(0, 0, 4, 2),
            Rect::new(0, 0, 10, 5),
            2,
        ))]);
        let mut readded = Vec::new();
        compositor
            .render(
                &mut readded,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        assert!(String::from_utf8(readded).unwrap().contains("a=t"));
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn cleanup_removes_all_transmitted_images() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(0, 0, 4, 2),
            Rect::new(0, 0, 10, 5),
            1,
        ))]);
        let mut compositor = TuiImageCompositor::new();
        compositor
            .render(
                &mut Vec::new(),
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 5,
                },
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();

        let mut output = Vec::new();
        compositor.cleanup(&mut output, &kitty_caps()).unwrap();

        assert!(String::from_utf8(output).unwrap().contains("a=d"));
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn hidden_and_revealed_image_deletes_then_retransmits() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(1, 1, 4, 2),
            Rect::new(0, 0, 10, 5),
            1,
        ))]);
        let mut compositor = TuiImageCompositor::new();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };
        compositor
            .render(
                &mut Vec::new(),
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();

        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(1, 1, 4, 2),
            Rect::new(8, 4, 2, 1),
            1,
        ))]);
        let mut hidden = Vec::new();
        compositor
            .render(
                &mut hidden,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        assert!(String::from_utf8(hidden).unwrap().contains("a=d"));

        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(1, 1, 4, 2),
            Rect::new(0, 0, 10, 5),
            1,
        ))]);
        let mut revealed = Vec::new();
        compositor
            .render(
                &mut revealed,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        assert!(String::from_utf8(revealed).unwrap().contains("a=t"));
    }

    #[test]
    fn resize_and_unsupported_hosts_are_bounded_and_silent() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(8, 3, 4, 2),
            Rect::new(0, 0, 20, 10),
            1,
        ))]);
        let mut compositor = TuiImageCompositor::new();
        let mut output = Vec::new();
        compositor
            .render(
                &mut output,
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 5,
                    h: 2,
                },
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        assert!(output.is_empty());

        compositor
            .render(
                &mut output,
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 20,
                    h: 10,
                },
                &HostImageCapabilities::default(),
                &ImageConfig::default(),
            )
            .unwrap();
        assert!(output.is_empty());
    }
}
