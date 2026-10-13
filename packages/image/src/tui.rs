//! Adapter from protocol-neutral TUI image scenes into BMUX image compositing.

use std::collections::BTreeMap;
use std::io::Write;

use bmux_tui::image::{ImageKey, ImagePayload as TuiImagePayload, ImagePixelFormat};
use bmux_tui::image_scene::{ImageScene, ImageSceneDelta};

use crate::compositor::{KittyHostState, PaneRect, render_image_clipped};
use crate::config::ImageConfig;
use crate::host_caps::HostImageCapabilities;
use crate::model::{
    ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, PaneImage,
    PixelBuffer, PixelFormat,
};
use std::sync::Arc;

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

type RetainedSource = (Arc<TuiImagePayload>, usize, Arc<PaneImage>);
type ContainSignature = (
    u64,
    bmux_tui::geometry::Rect,
    Option<bmux_tui::image::ImageBounds>,
    u16,
    u16,
);

/// BMUX-owned adapter that assigns image IDs, validates payloads, clips
/// placements, and delegates host protocol selection to the compositor.
#[derive(Default, Clone)]
pub struct TuiImageCompositor {
    next_id: u64,
    sources: BTreeMap<ImageKey, RetainedSource>,
    fitted: BTreeMap<ImageKey, (ContainSignature, Arc<PaneImage>)>,
    kitty_state: KittyHostState,
    reset_pending: bool,
    validations: usize,
    // Raster protocols have no reusable placement deletion. A prior footprint
    // requires ED2 followed by a complete cell repaint before the next overlay.
    raster_pending: bool,
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
            self.sources.remove(key);
        }
    }

    /// Render the current TUI image scene as a terminal overlay.
    /// Raster callers must first call [`Self::prepare_output`] on the same
    /// candidate before painting cells, and repaint all cells when it returns
    /// `true`. Commit the candidate only after the enclosing output flush.
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
        let mut candidate = self.clone();
        match candidate.render_candidate(out, scene, terminal, host_caps, config) {
            Ok(()) => {
                *self = candidate;
                Ok(())
            }
            Err(error) => {
                self.invalidate_after_failed_output(&candidate);
                Err(error)
            }
        }
    }

    /// Mark all potentially emitted host IDs for repair after write or flush failure.
    /// Call on committed state with the failed candidate; do not commit the candidate.
    pub fn invalidate_after_failed_output(&mut self, candidate: &Self) {
        self.raster_pending |= candidate.raster_pending;
        self.kitty_state
            .invalidate_after_failed_output(&candidate.kitty_state);
    }

    /// Clear uncertain/prior raster footprints before painting cells.
    /// Returns whether the caller must repaint its complete retained cell buffer.
    /// Use a candidate and commit only after the enclosing frame flush succeeds.
    ///
    /// # Errors
    /// Returns an output error; pending repair remains set on failure.
    pub fn prepare_output(&mut self, out: &mut impl Write) -> std::io::Result<bool> {
        if !self.raster_pending {
            return Ok(false);
        }
        out.write_all(b"\x1b[2J")?;
        self.raster_pending = false;
        // ED2 may discard Kitty resources too, notably after a protocol switch.
        self.kitty_state.reset(out)?;
        Ok(true)
    }

    fn render_candidate(
        &mut self,
        out: &mut impl Write,
        scene: &ImageScene,
        terminal: PaneRect,
        host_caps: &HostImageCapabilities,
        config: &ImageConfig,
    ) -> Result<(), TuiImageError> {
        self.kitty_state.begin_frame();
        if self.reset_pending {
            self.kitty_state.reset(out)?;
            self.reset_pending = false;
        }
        self.kitty_state.select_protocol(
            out,
            if config.enabled {
                host_caps.preferred_protocol()
            } else {
                None
            },
        )?;
        if !config.enabled || !host_caps.any_supported() {
            self.kitty_state.finish_frame(out)?;
            return Ok(());
        }

        self.kitty_state
            .select_protocol(out, host_caps.preferred_protocol())?;
        let terminal_clip =
            bmux_tui::geometry::Rect::new(terminal.x, terminal.y, terminal.w, terminal.h);
        for placement in scene.placements() {
            if placement
                .allocation
                .is_some_and(|bounds| !bounds.covers(placement.destination))
            {
                return Err(invalid(
                    &placement.key,
                    "allocation does not cover destination",
                ));
            }
            let mut output_caps = host_caps.clone();
            if placement.fit == bmux_tui::image::ImageFit::Contain {
                let (width, height) = host_caps.fitting_cell_size();
                output_caps.cell_pixel_width = width;
                output_caps.cell_pixel_height = height;
            }
            let host_caps = &output_caps;
            let Some(_) = clipped_destination(placement.destination, placement.clip, terminal_clip)
            else {
                continue;
            };
            let unchanged = self
                .sources
                .get(&placement.key)
                .is_some_and(|(payload, limit, _)| {
                    **payload == placement.payload && *limit == config.max_image_bytes
                });
            if !unchanged {
                let payload = decode_payload(&placement.key, &placement.payload, config)?;
                self.validations += 1;
                self.next_id = self
                    .next_id
                    .checked_add(1)
                    .expect("TUI image ID space exhausted");
                let pixels = payload.pixels.as_ref().expect("validated pixels");
                let image = Arc::new(PaneImage {
                    id: self.next_id,
                    protocol: ImageProtocol::KittyGraphics,
                    position: ImagePosition { row: 0, col: 0 },
                    cell_size: ImageCellSize::default(),
                    pixel_size: ImagePixelSize {
                        width: pixels.width,
                        height: pixels.height,
                    },
                    payload,
                });
                self.sources.insert(
                    placement.key.clone(),
                    (
                        Arc::new(placement.payload.clone()),
                        config.max_image_bytes,
                        image,
                    ),
                );
            }
            let source = Arc::clone(&self.sources.get(&placement.key).expect("retained source").2);
            let image = if placement.fit == bmux_tui::image::ImageFit::Contain {
                let signature = (
                    source.id,
                    placement.destination,
                    placement.allocation,
                    host_caps.cell_pixel_width.max(1),
                    host_caps.cell_pixel_height.max(1),
                );
                if !self
                    .fitted
                    .get(&placement.key)
                    .is_some_and(|(previous, _)| *previous == signature)
                {
                    let payload = contain_payload(placement, &source.payload, host_caps, config)?;
                    self.next_id = self
                        .next_id
                        .checked_add(1)
                        .expect("TUI image ID space exhausted");
                    let pixels = payload.pixels.as_ref().expect("contained pixels");
                    let fitted = Arc::new(PaneImage {
                        id: self.next_id,
                        pixel_size: ImagePixelSize {
                            width: pixels.width,
                            height: pixels.height,
                        },
                        payload,
                        ..(*source).clone()
                    });
                    self.fitted
                        .insert(placement.key.clone(), (signature, fitted));
                }
                Arc::clone(&self.fitted[&placement.key].1)
            } else {
                self.fitted.remove(&placement.key);
                source
            };
            if host_caps.preferred_protocol() != Some(ImageProtocol::KittyGraphics) {
                // Set before writes: even a partial raster sequence needs repair.
                self.raster_pending = true;
            }
            render_image_clipped(
                out,
                &image,
                placement.destination,
                placement.clip.intersection(terminal_clip),
                &[],
                host_caps,
                &mut self.kitty_state,
                Some(&image),
            )?;
            self.trim_sources();
        }
        self.kitty_state.finish_frame(out)?;
        self.sources.retain(|key, _| scene.get(key).is_some());
        self.fitted.retain(|key, _| self.sources.contains_key(key));
        self.trim_sources();
        Ok(())
    }

    fn trim_sources(&mut self) {
        // Source retention is derived and bounded; oversized scenes remain correct
        // but are revalidated on their next frame rather than retained unboundedly.
        while self.sources.len() > 128
            || self
                .fitted
                .values()
                .map(|(_, image)| image.payload.pixels.as_ref().map_or(0, |p| p.data.len()))
                .sum::<usize>()
                + self
                    .sources
                    .values()
                    .map(|(source, _, image)| {
                        let source_bytes = match &**source {
                            TuiImagePayload::Pixels { bytes, .. }
                            | TuiImagePayload::Png { bytes, .. } => bytes.len(),
                        };
                        source_bytes
                            + image.payload.raw.as_ref().map_or(0, Vec::len)
                            + image
                                .payload
                                .pixels
                                .as_ref()
                                .map_or(0, |pixels| pixels.data.len())
                    })
                    .sum::<usize>()
                > 64 * 1024 * 1024
        {
            self.sources.pop_first();
            self.fitted.retain(|key, _| self.sources.contains_key(key));
        }
    }

    /// Deterministic adapter validations, source decodes and fragment preparations.
    #[must_use]
    pub fn work_counts(&self) -> (usize, usize, usize) {
        let (decodes, preparations) = self.kitty_state.work_counts();
        (self.validations, decodes, preparations)
    }

    /// Queue every active image for protocol-specific removal.
    pub fn clear(&mut self) {
        self.reset_pending = true;
        self.sources.clear();
        self.fitted.clear();
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
        let _ = host_caps;
        let mut candidate = self.clone();
        candidate.clear();
        match candidate
            .prepare_output(out)
            .and_then(|_| candidate.kitty_state.reset(out))
        {
            Ok(()) => {
                candidate.reset_pending = false;
                *self = candidate;
                Ok(())
            }
            Err(error) => {
                self.invalidate_after_failed_output(&candidate);
                Err(error)
            }
        }
    }
}

// Rasterize the allocated fit once per source/geometry/metric revision. Letterbox
// pixels keep fractional-cell sizing exact; protocol emitters retain one geometry.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Values are bounded by the allocation and byte limit below.
fn contain_payload(
    placement: &bmux_tui::image::ImagePlacement,
    source: &ImagePayload,
    caps: &HostImageCapabilities,
    config: &ImageConfig,
) -> Result<ImagePayload, TuiImageError> {
    let (cw, ch) = caps.fitting_cell_size();
    let cw = u32::from(cw);
    let ch = u32::from(ch);
    let width = u32::from(placement.destination.width) * cw;
    let height = u32::from(placement.destination.height) * ch;
    let count = u64::from(width) * u64::from(height) * 4;
    if count > u64::try_from(config.max_image_bytes).unwrap_or(u64::MAX) {
        return Err(invalid(
            &placement.key,
            "contained output exceeds configured byte limit",
        ));
    }
    let fit = placement.pixel_rect(f64::from(cw), f64::from(ch));
    let fit_width = fit.width.round().max(1.0) as u32;
    let fit_height = fit.height.round().max(1.0) as u32;
    if u64::from(fit_width) * u64::from(fit_height) * 4
        > u64::try_from(config.max_image_bytes).unwrap_or(u64::MAX)
    {
        return Err(invalid(
            &placement.key,
            "fitted image exceeds configured byte limit",
        ));
    }
    let pixels = source.pixels.as_ref().expect("validated pixels");
    let decoded = match pixels.format {
        PixelFormat::Rgb8 => image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(pixels.width, pixels.height, pixels.data.clone())
                .expect("validated RGB"),
        ),
        PixelFormat::Rgba8 => image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(pixels.width, pixels.height, pixels.data.clone())
                .expect("validated RGBA"),
        ),
        PixelFormat::Png => image::ImageReader::with_format(
            std::io::Cursor::new(&pixels.data),
            image::ImageFormat::Png,
        )
        .decode()
        .map_err(|error| invalid(&placement.key, format!("PNG decode failed: {error}")))?,
    };
    let fitted = decoded
        .resize_exact(fit_width, fit_height, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let mut canvas = image::RgbaImage::new(width, height);
    image::imageops::overlay(
        &mut canvas,
        &fitted,
        (fit.x - f64::from(placement.destination.x) * f64::from(cw)).round() as i64,
        (fit.y - f64::from(placement.destination.y) * f64::from(ch)).round() as i64,
    );
    Ok(ImagePayload {
        raw: None,
        pixels: Some(PixelBuffer {
            width,
            height,
            format: PixelFormat::Rgba8,
            data: canvas.into_raw(),
        }),
    })
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

pub(crate) fn crop_to_visible(
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
    payload.raw = None;
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
            (bytes.clone(), *width, *height, PixelFormat::Png)
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
    #[cfg(feature = "sixel")]
    use std::sync::Arc;

    use crate::compositor::PaneRect;
    use crate::config::ImageConfig;
    use crate::host_caps::HostImageCapabilities;

    #[test]
    fn contain_letterboxes_and_crops_without_refitting() {
        let mut placed = placement(
            "contain",
            Rect::new(0, 0, 10, 6),
            Rect::new(0, 0, 10, 6),
            255,
        );
        placed.fit = bmux_tui::image::ImageFit::Contain;
        let config = ImageConfig::default();
        let mut caps = HostImageCapabilities {
            cell_pixel_width: 4,
            cell_pixel_height: 8,
            ..HostImageCapabilities::default()
        };
        let source = super::decode_payload(&placed.key, &placed.payload, &config).unwrap();
        let first = super::contain_payload(&placed, &source, &caps, &config).unwrap();
        let pixels = first.pixels.unwrap();
        assert_eq!((pixels.width, pixels.height), (40, 48));
        // The 2:1 source fits at 40x20, centered with 14px letterboxes.
        assert!(pixels.data[..40 * 14 * 4].iter().all(|byte| *byte == 0));
        assert!(
            pixels.data[40 * 14 * 4..40 * 34 * 4]
                .iter()
                .all(|byte| *byte == 255)
        );
        placed.allocation = Some(bmux_tui::image::ImageBounds {
            x: 0,
            y: -2,
            width: 10,
            height: 6,
        });
        placed.destination = Rect::new(0, 0, 10, 4);
        let clipped = super::contain_payload(&placed, &source, &caps, &config)
            .unwrap()
            .pixels
            .unwrap();
        assert_eq!(clipped.data, pixels.data[40 * 16 * 4..]);
        caps.cell_pixel_height = 4;
        assert_eq!(
            super::contain_payload(&placed, &source, &caps, &config)
                .unwrap()
                .pixels
                .unwrap()
                .height,
            16
        );
    }

    #[cfg(all(feature = "kitty", feature = "sixel", feature = "iterm2"))]
    #[test]
    fn contain_renders_real_deck_pngs_across_protocols() {
        for bytes in [
            include_bytes!("../../../presentations/bmux-tour/assets/system.png").as_slice(),
            include_bytes!("../../../presentations/bmux-tour/assets/frame.png").as_slice(),
            include_bytes!("../../../presentations/bmux-tour/assets/ownership.png").as_slice(),
        ] {
            let decoded = image::load_from_memory(bytes).unwrap();
            let mut placed = placement(
                "deck-png",
                Rect::new(0, 0, 40, 12),
                Rect::new(0, 0, 40, 12),
                0,
            );
            placed.fit = bmux_tui::image::ImageFit::default();
            placed.payload = ImagePayload::Png {
                bytes: bytes.to_vec(),
                width: decoded.width(),
                height: decoded.height(),
            };
            let config = ImageConfig::default();
            let source = super::decode_payload(&placed.key, &placed.payload, &config).unwrap();
            assert_eq!(
                source.pixels.as_ref().unwrap().format,
                crate::model::PixelFormat::Png
            );
            let mut scene = ImageScene::default();
            scene.reconcile(&[ImageContribution::Present(placed)]);
            for (kitty_graphics, sixel, iterm2_inline) in [
                (true, false, false),
                (false, true, false),
                (false, false, true),
            ] {
                let caps = HostImageCapabilities {
                    kitty_graphics,
                    sixel,
                    iterm2_inline,
                    cell_pixel_width: 8,
                    cell_pixel_height: 16,
                    ..Default::default()
                };
                let mut compositor = TuiImageCompositor::new();
                let mut output = Vec::new();
                compositor
                    .render(
                        &mut output,
                        &scene,
                        PaneRect {
                            x: 0,
                            y: 0,
                            w: 40,
                            h: 12,
                        },
                        &caps,
                        &config,
                    )
                    .unwrap();
                assert!(!output.is_empty());
                let fitted = &compositor.fitted.values().next().unwrap().1;
                assert_eq!(
                    fitted.payload.pixels.as_ref().unwrap().format,
                    crate::model::PixelFormat::Rgba8
                );
                let id = fitted.id;
                compositor
                    .render(
                        &mut Vec::new(),
                        &scene,
                        PaneRect {
                            x: 0,
                            y: 0,
                            w: 40,
                            h: 12,
                        },
                        &caps,
                        &config,
                    )
                    .unwrap();
                assert_eq!(compositor.fitted.values().next().unwrap().1.id, id);
            }
        }
    }

    #[cfg(all(feature = "kitty", feature = "sixel", feature = "iterm2"))]
    #[test]
    fn contain_protocols_use_the_same_fitted_pixels() {
        let mut scene = ImageScene::default();
        let mut placed = placement(
            "protocol-fit",
            Rect::new(0, 0, 10, 6),
            Rect::new(0, 0, 10, 6),
            255,
        );
        placed.fit = bmux_tui::image::ImageFit::Contain;
        scene.reconcile(&[ImageContribution::Present(placed)]);
        let mut expected = None;
        for (kitty_graphics, sixel, iterm2_inline, marker) in [
            (true, false, false, "a=t"),
            (false, true, false, "\u{1b}P"),
            (false, false, true, "1337;File="),
        ] {
            let caps = HostImageCapabilities {
                kitty_graphics,
                sixel,
                iterm2_inline,
                cell_pixel_width: 4,
                cell_pixel_height: 8,
                ..Default::default()
            };
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
                        h: 6,
                    },
                    &caps,
                    &ImageConfig::default(),
                )
                .unwrap();
            assert!(String::from_utf8_lossy(&output).contains(marker));
            let pixels = compositor
                .fitted
                .values()
                .next()
                .unwrap()
                .1
                .payload
                .pixels
                .as_ref()
                .unwrap();
            assert_eq!((pixels.width, pixels.height), (40, 48));
            if let Some(previous) = &expected {
                assert_eq!(pixels, previous);
            }
            expected = Some(pixels.clone());
        }
    }

    #[test]
    fn contain_reuses_geometry_until_metrics_change() {
        let mut scene = ImageScene::default();
        let mut placed = placement("fit", Rect::new(0, 0, 10, 6), Rect::new(0, 0, 10, 6), 255);
        placed.fit = bmux_tui::image::ImageFit::Contain;
        scene.reconcile(&[ImageContribution::Present(placed)]);
        let mut compositor = TuiImageCompositor::new();
        let mut caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 4,
            cell_pixel_height: 8,
            ..HostImageCapabilities::default()
        };
        let config = ImageConfig::default();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 10,
            h: 6,
        };
        compositor
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        let id = compositor.fitted.values().next().unwrap().1.id;
        compositor
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        assert_eq!(id, compositor.fitted.values().next().unwrap().1.id);
        caps.cell_pixel_height = 10;
        compositor
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        assert_ne!(id, compositor.fitted.values().next().unwrap().1.id);
    }

    fn placement(key: &str, destination: Rect, clip: Rect, value: u8) -> ImagePlacement {
        ImagePlacement {
            fit: bmux_tui::image::ImageFit::Stretch,
            allocation: None,
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

    #[cfg(all(feature = "sixel", feature = "iterm2", feature = "kitty"))]
    #[test]
    fn raster_repair_survives_failed_output_and_protocol_switch() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "raster",
            Rect::new(0, 0, 4, 2),
            Rect::new(0, 0, 8, 4),
            255,
        ))]);
        let pane = PaneRect {
            x: 0,
            y: 0,
            w: 8,
            h: 4,
        };
        for caps in [
            HostImageCapabilities {
                sixel: true,
                ..Default::default()
            },
            HostImageCapabilities {
                iterm2_inline: true,
                ..Default::default()
            },
        ] {
            let mut committed = super::TuiImageCompositor::new();
            let mut failed = committed.clone();
            failed
                .render(
                    &mut Vec::new(),
                    &scene,
                    pane,
                    &caps,
                    &ImageConfig::default(),
                )
                .unwrap();
            // Model a failed enclosing flush after a complete raster write.
            committed.invalidate_after_failed_output(&failed);
            let mut retry = committed.clone();
            let mut output = Vec::new();
            assert!(retry.prepare_output(&mut output).unwrap());
            retry
                .render(
                    &mut output,
                    &scene,
                    pane,
                    &kitty_caps(),
                    &ImageConfig::default(),
                )
                .unwrap();
            assert!(output.starts_with(b"\x1b[2J"));
            assert!(String::from_utf8_lossy(&output).contains("Ga=t"));
            assert!(!retry.prepare_output(&mut Vec::new()).unwrap());
            // A failed cleanup flush cannot discard committed raster repair.
            let mut raster = super::TuiImageCompositor::new();
            raster
                .render(
                    &mut Vec::new(),
                    &scene,
                    pane,
                    &caps,
                    &ImageConfig::default(),
                )
                .unwrap();
            let mut cleanup = raster.clone();
            cleanup.cleanup(&mut Vec::new(), &caps).unwrap();
            raster.invalidate_after_failed_output(&cleanup);
            assert!(raster.prepare_output(&mut Vec::new()).unwrap());
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
            fit: bmux_tui::image::ImageFit::Stretch,
            allocation: None,
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
        assert!(!String::from_utf8(readded).unwrap().contains("a=t"));
    }

    #[cfg(feature = "sixel")]
    #[test]
    fn warm_tui_scene_and_clip_changes_do_not_repeat_source_work() {
        let mut scene = ImageScene::default();
        let mut placed = placement("diagram", Rect::new(0, 0, 4, 2), Rect::new(0, 0, 3, 2), 1);
        scene.reconcile(&[ImageContribution::Present(placed.clone())]);
        let caps = HostImageCapabilities {
            sixel: true,
            cell_pixel_width: 2,
            cell_pixel_height: 2,
            ..HostImageCapabilities::default()
        };
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };
        let config = ImageConfig::default();
        let mut compositor = TuiImageCompositor::new();
        compositor
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        assert_eq!(compositor.work_counts(), (1, 1, 1));
        let source = Arc::clone(&compositor.sources.get(&placed.key).unwrap().2);
        let mut candidate = compositor.clone();
        candidate
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        assert_eq!(candidate.work_counts(), (1, 1, 1));
        assert!(Arc::ptr_eq(
            &source,
            &candidate.sources.get(&placed.key).unwrap().2
        ));
        placed.clip.width = 2;
        scene.reconcile(&[ImageContribution::Present(placed.clone())]);
        candidate
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        assert_eq!(candidate.work_counts(), (1, 1, 2));
        placed.clip.width = 3;
        scene.reconcile(&[ImageContribution::Present(placed)]);
        candidate
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        assert_eq!(candidate.work_counts(), (1, 1, 2));
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn explicit_clear_releases_retained_content_before_same_payload_retransmission() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "diagram",
            Rect::new(0, 0, 4, 2),
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
        compositor.clear();
        let mut output = Vec::new();
        compositor
            .render(
                &mut output,
                &scene,
                terminal,
                &kitty_caps(),
                &ImageConfig::default(),
            )
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.find("a=d,d=I").unwrap() < output.find("a=t").unwrap());
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
    fn hidden_and_revealed_image_reuses_retained_content() {
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
        assert!(!String::from_utf8(revealed).unwrap().contains("a=t"));
    }

    #[test]
    fn malformed_and_oversized_payloads_fail_without_emitting_output() {
        let cases = [
            (
                ImagePayload::Png {
                    bytes: vec![0, 1, 2, 3],
                    width: 1,
                    height: 1,
                },
                ImageConfig::default(),
            ),
            (
                ImagePayload::Pixels {
                    bytes: vec![0; 16],
                    width: 2,
                    height: 2,
                    format: ImagePixelFormat::Rgba8,
                },
                ImageConfig {
                    max_image_bytes: 8,
                    ..ImageConfig::default()
                },
            ),
        ];
        for (payload, config) in cases {
            let mut scene = ImageScene::default();
            scene.reconcile(&[ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
                key: ImageKey::new("invalid"),
                payload,
                destination: Rect::new(0, 0, 1, 1),
                clip: Rect::new(0, 0, 10, 5),
                lifecycle: ImageLifecycle::Frame,
            })]);
            let mut output = Vec::new();
            let result = TuiImageCompositor::new().render(
                &mut output,
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 5,
                },
                &kitty_caps(),
                &config,
            );
            assert!(matches!(
                result,
                Err(super::TuiImageError::InvalidPayload { .. })
            ));
            assert!(output.is_empty());
        }
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
