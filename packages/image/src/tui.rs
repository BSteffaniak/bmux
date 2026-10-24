//! Adapter from protocol-neutral TUI image scenes into BMUX image compositing.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;

use bmux_tui::image::{ImageKey, ImagePayload as TuiImagePayload, ImagePixelFormat};
use bmux_tui::image_scene::{ImageScene, ImageSceneDelta};

use crate::compositor::{KittyHostState, PaneRect, render_image_clipped};
use crate::config::ImageConfig;
use crate::frozen::FrozenImage;
use crate::host_caps::HostImageCapabilities;
use crate::model::{
    ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, PaneImage,
    PixelBuffer, PixelFormat,
};
use std::sync::Arc;

type RetainedImage = Arc<FrozenImage>;

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
    /// Cooperative image encoding cancellation.
    Cancelled,
    /// Image output failed.
    Io(std::io::Error),
}

impl std::fmt::Display for TuiImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPayload { key, reason } => {
                write!(formatter, "invalid TUI image '{}': {reason}", key.as_str())
            }
            Self::Cancelled => formatter.write_str("image encoding cancelled"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TuiImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPayload { .. } | Self::Cancelled => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for TuiImageError {
    fn from(value: std::io::Error) -> Self {
        if value.kind() == std::io::ErrorKind::Interrupted {
            Self::Cancelled
        } else {
            Self::Io(value)
        }
    }
}

type RetainedSource = (Arc<TuiImagePayload>, usize, RetainedImage);
type ContainSignature = (
    u64,
    bmux_tui::geometry::Rect,
    (u16, u16),
    u16,
    u16,
    bmux_tui::image::ImageFit,
);
type RasterSignature = (u64, u16, u16, u16, u16, bmux_tui::image::ImageFit);

/// BMUX-owned adapter that assigns image IDs, validates payloads, clips
/// placements, and delegates host protocol selection to the compositor.
#[derive(Default, Clone)]
pub struct TuiImageCompositor {
    next_id: u64,
    sources: BTreeMap<ImageKey, RetainedSource>,
    // Access order, not caller key order: shared hot images survive page changes.
    source_order: VecDeque<ImageKey>,
    fitted: BTreeMap<ImageKey, (ContainSignature, RetainedImage)>,
    // Full allocation pixels survive changes to screen position and visible crop.
    rasters: BTreeMap<ImageKey, (RasterSignature, Arc<ImagePayload>)>,
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

    /// Explicitly opt in to a physical-host-local Kitty SHM probe. Append to
    /// the host's serialized output queue, never a concurrent stdout writer.
    /// Returns false without output when unsupported/setup is unavailable.
    /// Call once per connection; TERM/SSH/loopback are not locality evidence.
    ///
    /// # Errors
    /// * Identifier exhaustion or output failure. Use
    ///   `invalidate_after_failed_output` after failed candidate delivery/flush;
    ///   abort is reserved for physical connection teardown.
    #[cfg(feature = "kitty")]
    pub fn begin_shared_memory_probe(
        &mut self,
        out: &mut impl Write,
        now: std::time::Instant,
    ) -> std::io::Result<bool> {
        self.kitty_state.begin_shared_memory_probe(out, now)
    }

    /// Route a complete APC frame from this physical connection. Returns true
    /// only for a matching pending reply. Runtime owns framing and must preserve
    /// unmatched input. Candidate clones share ACK/lease state, not scene commits;
    /// independent connections must use independently constructed compositors.
    #[cfg(feature = "kitty")]
    #[must_use]
    pub fn handle_shared_memory_reply(&mut self, frame: &[u8], now: std::time::Instant) -> bool {
        self.kitty_state.handle_shared_memory_reply(frame, now)
    }

    /// Run on the host timer even without image output. Releases expired pending
    /// leases (16 objects / 16 MiB / two seconds) and schedules residency repair.
    #[cfg(feature = "kitty")]
    pub fn expire_shared_memory(&mut self, now: std::time::Instant) {
        self.kitty_state.expire_shared_memory(now);
    }

    /// Release leases and disable SHM on physical connection teardown only.
    /// For failed candidate output/flush, use `invalidate_after_failed_output`:
    /// older uploads may still be read by the terminal and retain their leases.
    /// SIGKILL before terminal consumption can leave objects behind; there is no
    /// orphan scavenger and no foreign objects are deliberately reclaimed.
    #[cfg(feature = "kitty")]
    pub fn abort_shared_memory(&mut self) {
        self.kitty_state.abort_shared_memory();
    }

    /// Observe lifecycle removals without discarding bounded content preparation.
    /// Visibility is owned by the complete scene passed to render; finish_frame
    /// removes absent host placements. Inactive sources remain reusable until
    /// the entry/byte budgets evict them, or clear resets the adapter.
    pub fn apply_delta(&mut self, _delta: &ImageSceneDelta) {}

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
        self.render_cancellable(out, scene, terminal, host_caps, config, None)
    }

    /// Render with an optional live worker cancellation token. Cancellation
    /// discards candidate image state. PNG/resampling checkpoint coverage remains
    /// incomplete; this API alone does not guarantee bounded worker shutdown.
    ///
    /// # Errors
    /// * `Cancelled` when encoding observes the cancellation token.
    /// * The same payload/output errors as `render`.
    pub fn render_cancellable(
        &mut self,
        out: &mut impl Write,
        scene: &ImageScene,
        terminal: PaneRect,
        host_caps: &HostImageCapabilities,
        config: &ImageConfig,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<(), TuiImageError> {
        if cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(TuiImageError::Cancelled);
        }
        let mut candidate = self.clone();
        candidate.kitty_state.set_cancellation(cancelled);
        let result = candidate.render_candidate(out, scene, terminal, host_caps, config);
        let result = result.and_then(|()| {
            candidate
                .kitty_state
                .check_cancelled()
                .map_err(TuiImageError::from)
        });
        let result = if candidate.kitty_state.check_cancelled().is_err() {
            Err(TuiImageError::Cancelled)
        } else {
            result
        };
        candidate.kitty_state.set_cancellation(None);
        match result {
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
        // Scene order is not residency priority. Protect warm sources and fitted
        // representations before an early new upload can evict a later image.
        for placement in scene.placements() {
            if clipped_destination(placement.destination, placement.clip, terminal_clip).is_none() {
                continue;
            }
            if let Some((payload, limit, source)) = self.sources.get(&placement.key)
                && **payload == placement.payload
                && *limit == config.max_image_bytes
            {
                self.kitty_state.protect_retained(source.id);
                if let Some((_, fitted)) = self.fitted.get(&placement.key) {
                    self.kitty_state.protect_retained(fitted.id);
                }
                self.source_order.retain(|key| key != &placement.key);
                self.source_order.push_back(placement.key.clone());
            }
        }
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
                let payload = decode_payload_cancellable(
                    &placement.key,
                    &placement.payload,
                    config,
                    self.kitty_state.cancellation().as_deref(),
                )?;
                self.validations += 1;
                self.next_id = self
                    .next_id
                    .checked_add(1)
                    .expect("TUI image ID space exhausted");
                let pixels = payload.pixels.as_ref().expect("validated pixels");
                let image = FrozenImage::new(PaneImage {
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
            self.source_order.retain(|key| key != &placement.key);
            self.source_order.push_back(placement.key.clone());
            let source = Arc::clone(&self.sources.get(&placement.key).expect("retained source").2);
            #[cfg(feature = "kitty")]
            if placement.fit == bmux_tui::image::ImageFit::Contain
                && host_caps.preferred_protocol() == Some(ImageProtocol::KittyGraphics)
                && self.kitty_state.contain_frozen_png(
                    out,
                    &source,
                    placement,
                    host_caps,
                    placement.clip.intersection(terminal_clip),
                )?
            {
                self.trim_sources();
                continue;
            }
            #[cfg(feature = "kitty")]
            if host_caps.preferred_protocol() == Some(ImageProtocol::KittyGraphics)
                && (placement.fit == bmux_tui::image::ImageFit::Contain
                    || placement.allocation.is_some())
            {
                let bounds = placement
                    .allocation
                    .unwrap_or_else(|| placement.destination.into());
                let mut allocation = placement.clone();
                allocation.destination =
                    bmux_tui::geometry::Rect::new(0, 0, bounds.width, bounds.height);
                allocation.allocation = None;
                let image = self.contained_image(&allocation, &source, host_caps, config)?;
                self.kitty_state.contained_raster(
                    out,
                    &image,
                    placement,
                    host_caps,
                    placement.clip.intersection(terminal_clip),
                )?;
                self.trim_sources();
                continue;
            }
            let image = if placement.fit == bmux_tui::image::ImageFit::Contain
                || placement.allocation.is_some()
            {
                self.contained_image(placement, &source, host_caps, config)?
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
                Some(image.shared()),
            )?;
            self.trim_sources();
        }
        self.kitty_state.finish_frame(out)?;
        // Keep inactive preparation within trim_sources' entry/byte budgets so
        // navigating away and back does not decode or resize the same resource.
        self.fitted.retain(|key, _| self.sources.contains_key(key));
        self.trim_sources();
        Ok(())
    }

    fn contained_image(
        &mut self,
        placement: &bmux_tui::image::ImagePlacement,
        source: &RetainedImage,
        caps: &HostImageCapabilities,
        config: &ImageConfig,
    ) -> Result<RetainedImage, TuiImageError> {
        let bounds = placement
            .allocation
            .unwrap_or_else(|| placement.destination.into());
        let relative = relative_destination(placement);
        let (cw, ch) = caps.fitting_cell_size();
        let signature = (
            source.id,
            relative,
            (bounds.width, bounds.height),
            cw,
            ch,
            placement.fit,
        );
        if let Some((previous, fitted)) = self.fitted.get(&placement.key)
            && *previous == signature
        {
            return Ok(Arc::clone(fitted));
        }
        let raster_signature = (
            source.id,
            bounds.width,
            bounds.height,
            cw,
            ch,
            placement.fit,
        );
        if self
            .rasters
            .get(&placement.key)
            .is_none_or(|(previous, _)| *previous != raster_signature)
        {
            let mut allocation = placement.clone();
            allocation.destination =
                bmux_tui::geometry::Rect::new(0, 0, bounds.width, bounds.height);
            allocation.allocation = None;
            let cancellation = self.kitty_state.cancellation();
            let raster = contain_payload_cancellable(
                &allocation,
                &source.payload,
                caps,
                config,
                cancellation.as_deref(),
            )?;
            self.rasters
                .insert(placement.key.clone(), (raster_signature, Arc::new(raster)));
        }
        let mut payload = (*self.rasters[&placement.key].1).clone();
        crop_to_visible(
            &mut payload,
            bmux_tui::geometry::Rect::new(0, 0, bounds.width, bounds.height),
            relative,
        );
        #[cfg(feature = "kitty")]
        if caps.preferred_protocol() == Some(ImageProtocol::KittyGraphics) {
            if self.kitty_state.cancellation().is_some() {
                let token = self.kitty_state.cancellation();
                let pixels = payload.pixels.as_ref().expect("fitted pixels");
                let png = crate::compositor::clipping::encode_rgba_png_cancellable(pixels, || {
                    token
                        .as_ref()
                        .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
                })?;
                if png.len() < pixels.data.len() {
                    payload.raw = Some(png);
                }
            } else {
                encode_fitted_png(&mut payload);
            }
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("TUI image ID space exhausted");
        let pixels = payload.pixels.as_ref().expect("contained pixels");
        let fitted = FrozenImage::new(PaneImage {
            id: self.next_id,
            pixel_size: ImagePixelSize {
                width: pixels.width,
                height: pixels.height,
            },
            payload,
            ..(***source).clone()
        });
        self.fitted
            .insert(placement.key.clone(), (signature, Arc::clone(&fitted)));
        Ok(fitted)
    }

    fn trim_sources(&mut self) {
        // Source retention is derived and bounded; oversized scenes remain correct
        // but are revalidated on their next frame rather than retained unboundedly.
        while self.sources.len() > 128
            || self
                .fitted
                .values()
                .map(|(_, image)| {
                    image.payload.pixels.as_ref().map_or(0, |p| p.data.len())
                        + image.payload.raw.as_ref().map_or(0, Vec::len)
                })
                .sum::<usize>()
                + self
                    .rasters
                    .values()
                    .map(|(_, payload)| payload.pixels.as_ref().map_or(0, |p| p.data.len()))
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
            let key = self
                .source_order
                .pop_front()
                .expect("retained source order");
            self.sources.remove(&key);
            self.rasters.retain(|key, _| self.sources.contains_key(key));
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
        self.source_order.clear();
        self.fitted.clear();
        self.rasters.clear();
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

// Retain a lossless wire representation only when smaller than RGBA. Fixed
// output storage bounds even incompressible/adversarial input to the raster size.
#[cfg(feature = "kitty")]
fn encode_fitted_png(payload: &mut ImagePayload) {
    encode_fitted_png_with_compression(payload, image::codecs::png::CompressionType::Default);
}

#[cfg(feature = "kitty")]
fn encode_fitted_png_with_compression(
    payload: &mut ImagePayload,
    compression: image::codecs::png::CompressionType,
) {
    use image::ImageEncoder;
    let pixels = payload.pixels.as_ref().expect("fitted pixels");
    let mut storage = vec![0; pixels.data.len()];
    let mut output = std::io::Cursor::new(storage.as_mut_slice());
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut output,
        compression,
        image::codecs::png::FilterType::Adaptive,
    );
    if encoder
        .write_image(
            &pixels.data,
            pixels.width,
            pixels.height,
            image::ExtendedColorType::Rgba8,
        )
        .is_ok()
    {
        let length = usize::try_from(output.position()).expect("bounded PNG length");
        if length < pixels.data.len() {
            storage.truncate(length);
            payload.raw = Some(storage);
        }
    }
}

// Rasterize the allocated fit once per source/geometry/metric revision. Letterbox
// pixels keep fractional-cell sizing exact; protocol emitters retain one geometry.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Values are bounded by the allocation and byte limit below.
#[cfg(test)]
fn contain_payload(
    placement: &bmux_tui::image::ImagePlacement,
    source: &ImagePayload,
    caps: &HostImageCapabilities,
    config: &ImageConfig,
) -> Result<ImagePayload, TuiImageError> {
    contain_payload_cancellable(placement, source, caps, config, None)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Bounded allocation coordinates.
fn contain_payload_cancellable(
    placement: &bmux_tui::image::ImagePlacement,
    source: &ImagePayload,
    caps: &HostImageCapabilities,
    config: &ImageConfig,
    cancelled: Option<&std::sync::atomic::AtomicBool>,
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
        PixelFormat::Png if cancelled.is_some() => {
            let token = cancelled.expect("checked token");
            let rgba = crate::compositor::clipping::decode_png_cancellable(
                &pixels.data,
                config.max_image_bytes,
                || token.load(std::sync::atomic::Ordering::Acquire),
            )?;
            image::DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(rgba.width, rgba.height, rgba.data)
                    .expect("validated PNG rows"),
            )
        }
        PixelFormat::Png => image::ImageReader::with_format(
            std::io::Cursor::new(&pixels.data),
            image::ImageFormat::Png,
        )
        .decode()
        .map_err(|error| invalid(&placement.key, format!("PNG decode failed: {error}")))?,
    };
    let fitted = if let Some(token) = cancelled {
        crate::compositor::clipping::resize_rgba_cancellable(
            &decoded.into_rgba8(),
            fit_width,
            fit_height,
            || token.load(std::sync::atomic::Ordering::Acquire),
        )?
    } else {
        decoded
            .resize_exact(fit_width, fit_height, image::imageops::FilterType::Lanczos3)
            .into_rgba8()
    };
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

// Callers validate allocation coverage before rendering, so relative coordinates
// are nonnegative and bounded by the allocation's u16 dimensions.
fn relative_destination(placement: &bmux_tui::image::ImagePlacement) -> bmux_tui::geometry::Rect {
    let bounds = placement
        .allocation
        .unwrap_or_else(|| placement.destination.into());
    bmux_tui::geometry::Rect::new(
        u16::try_from(i64::from(placement.destination.x) - i64::from(bounds.x))
            .expect("validated allocation x"),
        u16::try_from(i64::from(placement.destination.y) - i64::from(bounds.y))
            .expect("validated allocation y"),
        placement.destination.width,
        placement.destination.height,
    )
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

#[cfg(test)]
fn decode_payload(
    key: &ImageKey,
    payload: &TuiImagePayload,
    config: &ImageConfig,
) -> Result<ImagePayload, TuiImageError> {
    decode_payload_cancellable(key, payload, config, None)
}

fn decode_payload_cancellable(
    key: &ImageKey,
    payload: &TuiImagePayload,
    config: &ImageConfig,
    cancelled: Option<&std::sync::atomic::AtomicBool>,
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
            let decoded = if let Some(token) = cancelled {
                let rgba = crate::compositor::clipping::decode_png_cancellable(
                    bytes,
                    config.max_image_bytes,
                    || token.load(std::sync::atomic::Ordering::Acquire),
                )?;
                image::DynamicImage::ImageRgba8(
                    image::RgbaImage::from_raw(rgba.width, rgba.height, rgba.data)
                        .expect("validated PNG rows"),
                )
            } else {
                image::ImageReader::with_format(
                    std::io::Cursor::new(bytes),
                    image::ImageFormat::Png,
                )
                .decode()
                .map_err(|error| invalid(key, format!("PNG decode failed: {error}")))?
            };
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

    #[cfg(all(feature = "kitty", target_os = "linux"))]
    #[test]
    fn shared_memory_public_hooks_share_candidate_ack_not_connection_eligibility() {
        let now = std::time::Instant::now();
        let caps = kitty_caps();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 40,
            h: 20,
        };
        let config = ImageConfig::default();
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "cover",
            Rect::new(0, 0, 8, 4),
            Rect::new(0, 0, 40, 20),
            91,
        ))]);
        let mut committed = TuiImageCompositor::new();
        let mut direct_candidate = committed.clone();
        let mut direct = Vec::new();
        direct_candidate
            .render(&mut direct, &scene, terminal, &caps, &config)
            .unwrap();
        assert!(String::from_utf8_lossy(&direct).contains("a=t"));
        assert!(!String::from_utf8_lossy(&direct).contains("t=s"));
        drop(direct_candidate);

        // Build probe bytes in the same output queue as ordinary frames. This
        // test never opens stdout or a TTY and never runs a concurrent writer.
        let mut ordered = b"previous-output".to_vec();
        assert!(
            committed
                .begin_shared_memory_probe(&mut ordered, now)
                .unwrap()
        );
        assert!(ordered.starts_with(b"previous-output\x1b\\\x1b_Ga=q,t=s"));
        let command = String::from_utf8_lossy(&ordered);
        let probe_id = command
            .split_once(",i=")
            .unwrap()
            .1
            .split(',')
            .next()
            .unwrap();
        let encoded = command
            .split_once(';')
            .unwrap()
            .1
            .strip_suffix("\x1b\\")
            .unwrap();
        let name =
            String::from_utf8(crate::codec::kitty::base64_decode(encoded.as_bytes())).unwrap();
        let path = std::path::Path::new("/dev/shm").join(&name[1..]);
        let mut candidate = committed.clone();
        let other = TuiImageCompositor::new();
        let mut other_candidate = other.clone();
        let ack = format!("\x1b_Gi={probe_id};OK\x1b\\");
        assert!(!other_candidate.handle_shared_memory_reply(ack.as_bytes(), now));
        assert!(!candidate.handle_shared_memory_reply(b"user input", now));
        assert!(path.exists());
        // ACK reaches the committed owner while a presentation clone exists.
        assert!(committed.handle_shared_memory_reply(ack.as_bytes(), now));
        assert!(!path.exists());
        let mut shm = Vec::new();
        candidate
            .render(&mut shm, &scene, terminal, &caps, &config)
            .unwrap();
        let shm = String::from_utf8(shm).unwrap();
        assert!(shm.contains("a=t,t=s"));
        assert!(shm.contains("a=p"));
        // Same 8x4 RGBA fixture and placement. This compares emitted bytes,
        // not latency, terminal rendering or system-wide memory traffic.
        assert!(shm.len() < direct.len());
        eprintln!(
            "same-image wire bytes: direct={} shm={} probe={}",
            direct.len(),
            shm.len(),
            ordered.len() - b"previous-output".len()
        );
        let upload = shm.split_once("a=t,t=s").unwrap().1;
        let encoded = upload
            .split_once(';')
            .unwrap()
            .1
            .split_once("\x1b\\")
            .unwrap()
            .0;
        let name =
            String::from_utf8(crate::codec::kitty::base64_decode(encoded.as_bytes())).unwrap();
        let upload_path = std::path::Path::new("/dev/shm").join(&name[1..]);
        assert_eq!(std::fs::read(&upload_path).unwrap(), vec![91; 8 * 4 * 4]);
        let mut isolated = Vec::new();
        other_candidate
            .render(&mut isolated, &scene, terminal, &caps, &config)
            .unwrap();
        assert!(!String::from_utf8_lossy(&isolated).contains("t=s"));
        drop(candidate); // Pending upload lease remains owned by committed.
        assert!(upload_path.exists());
        committed.expire_shared_memory(now + std::time::Duration::from_secs(3));
        assert!(!upload_path.exists());
        committed.abort_shared_memory();
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn render_cancellable_precancel_does_not_advance_actual_image_state() {
        use std::sync::atomic::AtomicBool;
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "cancelled-cover",
            Rect::new(0, 0, 8, 4),
            Rect::new(0, 0, 40, 20),
            91,
        ))]);
        let cancelled = std::sync::Arc::new(AtomicBool::new(true));
        let mut bytes = b"existing-output".to_vec();
        let error = compositor
            .render_cancellable(
                &mut bytes,
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 40,
                    h: 20,
                },
                &kitty_caps(),
                &ImageConfig::default(),
                Some(cancelled),
            )
            .unwrap_err();
        assert!(matches!(error, super::TuiImageError::Cancelled));
        assert_eq!(bytes, b"existing-output");
        assert!(compositor.sources.is_empty());
        assert!(compositor.fitted.is_empty());
        assert!(compositor.rasters.is_empty());
        assert!(compositor.kitty_state.resources.is_empty());
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn actual_render_cancellation_after_output_begins_does_not_commit_sources() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct CancelOnWrite(std::sync::Arc<AtomicBool>);
        impl std::io::Write for CancelOnWrite {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.store(true, Ordering::Release);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "cancel-cover",
            Rect::new(0, 0, 8, 4),
            Rect::new(0, 0, 40, 20),
            91,
        ))]);
        let error = compositor
            .render_cancellable(
                &mut CancelOnWrite(flag.clone()),
                &scene,
                PaneRect {
                    x: 0,
                    y: 0,
                    w: 40,
                    h: 20,
                },
                &kitty_caps(),
                &ImageConfig::default(),
                Some(flag),
            )
            .unwrap_err();
        assert!(matches!(error, super::TuiImageError::Cancelled));
        assert!(compositor.sources.is_empty());
        assert!(compositor.kitty_state.resources.is_empty());
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn placement_keys_share_content_not_geometry_or_lifetime() {
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 1,
            cell_pixel_height: 1,
            ..Default::default()
        };
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 40,
            h: 20,
        };
        let config = ImageConfig::default();
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        let mut first = placement("first", Rect::new(3, 2, 8, 4), Rect::new(0, 0, 40, 20), 37);
        first.allocation = Some(first.destination.into());
        let mut second = first.clone();
        second.key = ImageKey::new("second");
        second.clip = Rect::new(3, 2, 4, 2);
        scene.reconcile(&[
            ImageContribution::Present(first.clone()),
            ImageContribution::Present(second.clone()),
        ]);
        let mut output = Vec::new();
        compositor
            .render(&mut output, &scene, terminal, &caps, &config)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("a=t").count(), 1);
        assert_eq!(output.matches("a=p").count(), 2);
        assert_eq!(output.matches("\x1b[3;4H").count(), 4);
        assert!(output.contains("x=0,y=0,w=8,h=4"));
        assert!(output.contains("x=0,y=0,w=4,h=2"));
        assert_eq!(compositor.kitty_state.resources.len(), 1);

        scene.reconcile(&[ImageContribution::Present(second)]);
        let mut output = Vec::new();
        compositor
            .render(&mut output, &scene, terminal, &caps, &config)
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("a=t"));
        assert_eq!(output.matches("a=p").count(), 1);
        assert_eq!(output.matches("a=d,d=i").count(), 1);
        assert!(!output.contains("a=d,d=I"));
        assert!(output.contains("x=0,y=0,w=4,h=2"));
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn deduplicated_candidate_write_and_flush_failures_do_not_commit() {
        struct FailAfter(usize);
        impl std::io::Write for FailAfter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::other("injected partial output"));
                }
                let written = self.0.min(bytes.len());
                self.0 -= written;
                Ok(written)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("injected flush failure"))
            }
        }
        let caps = kitty_caps();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 40,
            h: 20,
        };
        let config = ImageConfig::default();
        let mut committed = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        scene.reconcile(&[
            ImageContribution::Present(placement(
                "first",
                Rect::new(0, 0, 8, 4),
                Rect::new(0, 0, 40, 20),
                91,
            )),
            ImageContribution::Present(placement(
                "second",
                Rect::new(0, 0, 8, 4),
                Rect::new(0, 0, 40, 20),
                91,
            )),
        ]);
        assert!(
            committed
                .render(&mut FailAfter(20), &scene, terminal, &caps, &config)
                .is_err()
        );
        assert!(committed.sources.is_empty());
        assert!(committed.kitty_state.resources.is_empty());
        // Outer presentation owners stage successful writes until flush succeeds.
        let mut candidate = committed.clone();
        let mut bytes = Vec::new();
        candidate
            .render(&mut bytes, &scene, terminal, &caps, &config)
            .unwrap();
        assert!(std::io::Write::flush(&mut FailAfter(0)).is_err());
        committed.invalidate_after_failed_output(&candidate);
        assert!(committed.kitty_state.resources.is_empty());
        assert!(committed.sources.is_empty());
        let mut retry = Vec::new();
        committed
            .render(&mut retry, &scene, terminal, &caps, &config)
            .unwrap();
        let retry = String::from_utf8(retry).unwrap();
        assert!(retry.find("d=I").unwrap() < retry.find("a=t").unwrap());
        assert_eq!(retry.matches("a=t").count(), 1);
        assert_eq!(retry.matches("a=p").count(), 2);
        assert_eq!(committed.kitty_state.resources.len(), 1);
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn frozen_warm_candidate_forks_repair_and_retry_without_stale_binding() {
        let caps = kitty_caps();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 40,
            h: 20,
        };
        let config = ImageConfig::default();
        let mut placed = placement(
            "frozen-fork",
            Rect::new(0, 0, 8, 4),
            Rect::new(0, 0, 40, 20),
            0,
        );
        placed.payload = transition_png();
        placed.fit = bmux_tui::image::ImageFit::Contain;
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placed.clone())]);
        let mut committed = TuiImageCompositor::new();
        committed
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        // Both forks share immutable preparation but not committed residency.
        let mut success = committed.clone();
        let mut discarded = committed.clone();
        placed.destination.x = 1;
        scene.reconcile(&[ImageContribution::Present(placed)]);
        let mut success_bytes = Vec::new();
        success
            .render(&mut success_bytes, &scene, terminal, &caps, &config)
            .unwrap();
        discarded
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        // Model enclosing flush failure: its bytes may have reached the host.
        committed.invalidate_after_failed_output(&discarded);
        let mut retry_bytes = Vec::new();
        committed
            .render(&mut retry_bytes, &scene, terminal, &caps, &config)
            .unwrap();
        let retry = String::from_utf8_lossy(&retry_bytes);
        assert!(retry.find("d=I").unwrap() < retry.find("a=t").unwrap());
        // The retry rebound the shared hint to its new generation. The success
        // fork must reject that hint, revalidate exactly, then remain reusable.
        let mut fork_bytes = Vec::new();
        success
            .render(&mut fork_bytes, &scene, terminal, &caps, &config)
            .unwrap();
        assert!(String::from_utf8_lossy(&fork_bytes).contains("a=p"));
        assert!(!String::from_utf8_lossy(&fork_bytes).contains("a=t"));
        crate::model::test_work::take();
        success
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        let work = crate::model::test_work::take();
        assert_eq!(work.hash_bytes, 0);
        assert_eq!(work.equality_bytes, 0);
    }

    #[cfg(feature = "kitty")]
    fn distinct_pressure_png(value: u8) -> ImagePayload {
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        let mut pixels = vec![value; 8 * 4 * 4];
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&pixels, 8, 4, image::ColorType::Rgba8.into())
            .unwrap();
        ImagePayload::Png {
            bytes,
            width: 8,
            height: 4,
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn contain_predeclares_late_sources_and_fitted_rasters_before_upload() {
        for fitted in [false, true] {
            let caps = HostImageCapabilities {
                kitty_graphics: true,
                cell_pixel_width: 1,
                cell_pixel_height: 1,
                ..Default::default()
            };
            let terminal = PaneRect {
                x: 0,
                y: 0,
                w: 80,
                h: 40,
            };
            let config = ImageConfig::default();
            let mut compositor = TuiImageCompositor::new();
            let mut scene = ImageScene::default();
            let mut hot = placement(
                "z-hot",
                Rect::new(0, 0, if fitted { 9 } else { 8 }, 4),
                Rect::new(0, 0, 80, 40),
                0,
            );
            hot.fit = bmux_tui::image::ImageFit::Contain;
            hot.payload = transition_png();
            // Make the hot image the oldest host ID, despite its late scene key.
            scene.reconcile(&[ImageContribution::Present(hot.clone())]);
            compositor
                .render(&mut Vec::new(), &scene, terminal, &caps, &config)
                .unwrap();
            let hot_id = if fitted {
                compositor.fitted[&hot.key].1.id
            } else {
                compositor.sources[&hot.key].2.id
            };
            let hot_host = compositor.kitty_state.transmitted[&hot_id];
            let mut all = vec![ImageContribution::Present(hot.clone())];
            for index in 0..127 {
                let mut cold = hot.clone();
                cold.key = ImageKey::new(format!("b-cold-{index:03}"));
                // Residency pressure requires distinct content, not aliases of
                // the hot PNG (equal payloads now deliberately share a resource).
                cold.payload = distinct_pressure_png(u8::try_from(index).unwrap());
                all.push(ImageContribution::Present(cold));
            }
            scene.reconcile(&all);
            compositor
                .render(&mut Vec::new(), &scene, terminal, &caps, &config)
                .unwrap();
            assert_eq!(compositor.kitty_state.transmitted.len(), 128);
            let before = compositor.work_counts();
            let mut new = hot.clone();
            new.key = ImageKey::new("a-new");
            new.payload = distinct_pressure_png(200);
            scene.reconcile(&[
                ImageContribution::Present(new),
                ImageContribution::Present(hot),
            ]);
            let mut output = Vec::new();
            compositor
                .render(&mut output, &scene, terminal, &caps, &config)
                .unwrap();
            assert_eq!(compositor.kitty_state.transmitted[&hot_id], hot_host);
            assert_eq!(compositor.work_counts().0, before.0 + 1);
            assert_eq!(String::from_utf8_lossy(&output).matches("a=t").count(), 1);
            assert_eq!(compositor.kitty_state.transmitted.len(), 128);
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn contain_hot_sources_survive_page_cache_pressure() {
        let caps = kitty_caps();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 80,
            h: 40,
        };
        let config = ImageConfig::default();
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        let mut hot = placement("a-hot", Rect::new(0, 0, 8, 4), Rect::new(0, 0, 80, 40), 7);
        hot.fit = bmux_tui::image::ImageFit::Contain;
        hot.payload = transition_png();
        for page in 0..140 {
            let mut cold = hot.clone();
            cold.key = ImageKey::new(format!("z-page-{page:03}"));
            cold.destination.y = 8;
            scene.reconcile(&[
                ImageContribution::Present(hot.clone()),
                ImageContribution::Present(cold),
            ]);
            compositor
                .render(&mut Vec::new(), &scene, terminal, &caps, &config)
                .unwrap();
        }
        let before = compositor.work_counts();
        let mut output = Vec::new();
        compositor
            .render(&mut output, &scene, terminal, &caps, &config)
            .unwrap();
        eprintln!(
            "warm cache pressure validations: {} -> {}; output bytes: {}",
            before.0,
            compositor.work_counts().0,
            output.len()
        );
        assert_eq!(
            compositor.work_counts(),
            before,
            "warm sources must not be revalidated after visiting >128 resources"
        );
        assert!(!String::from_utf8_lossy(&output).contains("a=t"));
        assert!(compositor.sources.len() <= 128);
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn retained_allocation_scroll_preserves_fit_pixels_and_upload() {
        // Cover is represented by the adapter's pre-cropped payload + Stretch.
        for fit in [
            bmux_tui::image::ImageFit::Contain,
            bmux_tui::image::ImageFit::Stretch,
        ] {
            let caps = HostImageCapabilities {
                kitty_graphics: true,
                cell_pixel_width: 1,
                cell_pixel_height: 1,
                ..Default::default()
            };
            let terminal = PaneRect {
                x: 0,
                y: 0,
                w: 20,
                h: 20,
            };
            let config = ImageConfig::default();
            let mut compositor = TuiImageCompositor::new();
            let mut scene = ImageScene::default();
            let mut placed = placement(
                "retained",
                Rect::new(0, 0, 7, 5),
                Rect::new(0, 0, 20, 20),
                0,
            );
            placed.payload = transition_png();
            placed.fit = fit;
            let mut uploaded = std::collections::BTreeMap::new();
            let mut source_id = None;
            for offset in [0, 1, 2, 1, 0] {
                placed.allocation = Some(bmux_tui::image::ImageBounds {
                    x: 0,
                    y: -offset,
                    width: 7,
                    height: 5,
                });
                placed.destination.height = 5 - u16::try_from(offset).unwrap();
                scene.reconcile(&[ImageContribution::Present(placed.clone())]);
                let mut output = Vec::new();
                compositor
                    .render(&mut output, &scene, terminal, &caps, &config)
                    .unwrap();
                let commands = String::from_utf8(output).unwrap();
                let placements = displayable_kitty_placements(&commands, &mut uploaded);
                assert_eq!(placements.len(), 1);
                let full = &compositor.fitted[&placed.key].1;
                if let Some(id) = source_id {
                    assert_eq!(full.id, id);
                    assert!(!commands.contains("a=t"));
                }
                source_id = Some(full.id);
                let crop = placements[0].source_rect.unwrap();
                let pixels = full.payload.pixels.as_ref().unwrap();
                let full =
                    image::RgbaImage::from_raw(pixels.width, pixels.height, pixels.data.clone())
                        .unwrap();
                let actual =
                    image::imageops::crop_imm(&full, crop.x, crop.y, crop.width, crop.height)
                        .to_image();
                let source = &compositor.sources[&placed.key].2;
                let expected =
                    super::contain_payload(&placed, &source.payload, &caps, &config).unwrap();
                assert_eq!(
                    actual.as_raw(),
                    &expected.pixels.unwrap().data,
                    "fit={fit:?}, offset={offset}"
                );
            }
            assert_eq!(uploaded.len(), 1);
        }
    }

    /// Adapter-owned, unchanged representations must not rescan wire bytes on
    /// warm frames. Keep this separate from the generic mutable PaneImage path,
    /// which must continue checking content rather than trusting owner IDs.
    #[cfg(feature = "kitty")]
    #[test]
    fn warm_owned_png_placement_does_not_hash_or_compare_payload_bytes() {
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 1,
            cell_pixel_height: 1,
            ..Default::default()
        };
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 40,
            h: 20,
        };
        let config = ImageConfig::default();
        let mut scene = ImageScene::default();
        let mut placed = placement(
            "frozen-warm",
            Rect::new(0, 0, 8, 4),
            Rect::new(0, 0, 40, 20),
            0,
        );
        placed.payload = transition_png();
        placed.fit = bmux_tui::image::ImageFit::Contain;
        scene.reconcile(&[ImageContribution::Present(placed.clone())]);
        let mut committed = TuiImageCompositor::new();
        committed
            .render(&mut Vec::new(), &scene, terminal, &caps, &config)
            .unwrap();
        let preparation = committed.work_counts();
        for x in [1, 2, 1, 0] {
            placed.destination.x = x;
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            crate::model::test_work::take();
            let mut candidate = committed.clone();
            let mut output = Vec::new();
            candidate
                .render(&mut output, &scene, terminal, &caps, &config)
                .unwrap();
            let work = crate::model::test_work::take();
            assert_eq!(candidate.work_counts(), preparation);
            assert!(!String::from_utf8_lossy(&output).contains("a=t"));
            assert_eq!(work.clone_bytes, 0);
            assert_eq!(work.hash_bytes, 0, "warm owned PNG must not be rehashed");
            assert_eq!(
                work.equality_bytes, 0,
                "warm owned PNG must not repeat canonical byte equality"
            );
            committed = candidate;
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn contain_stable_fragment_keys_reuse_sources_across_scroll() {
        let caps = kitty_caps();
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 80,
            h: 40,
        };
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        let mut before = None;
        for y in [10, 9, 8, 9, 10] {
            let mut parts = Vec::new();
            for (ordinal, x) in [0, 4].into_iter().enumerate() {
                let mut part = placement(
                    &format!("image/fragment-{ordinal}"),
                    Rect::new(x, y, 3, 3),
                    Rect::new(0, 0, 80, 40),
                    0,
                );
                part.payload = transition_png();
                part.fit = bmux_tui::image::ImageFit::Contain;
                part.allocation = Some(bmux_tui::image::ImageBounds {
                    x: 0,
                    y: i32::from(y),
                    width: 8,
                    height: 4,
                });
                parts.push(ImageContribution::Present(part));
            }
            scene.reconcile(&parts);
            let mut output = Vec::new();
            compositor
                .render(
                    &mut output,
                    &scene,
                    terminal,
                    &caps,
                    &ImageConfig::default(),
                )
                .unwrap();
            if let Some(counts) = before {
                assert_eq!(compositor.work_counts(), counts);
                assert!(!String::from_utf8_lossy(&output).contains("a=t"));
            }
            before = Some(compositor.work_counts());
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn contain_native_raster_alignment_transition_reuses_both_resources() {
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 1,
            cell_pixel_height: 1,
            ..Default::default()
        };
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 20,
            h: 20,
        };
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        let mut uploaded = std::collections::BTreeMap::new();
        let mut placed = placement(
            "alignment",
            Rect::new(0, 0, 8, 4),
            Rect::new(0, 0, 20, 20),
            0,
        );
        placed.fit = bmux_tui::image::ImageFit::Contain;
        placed.payload = transition_png();
        // 8x4 is native; 9x4 has a half-cell letterbox and needs a raster.
        for (index, width) in [8, 9, 8, 9, 8].into_iter().enumerate() {
            placed.destination.width = width;
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            let mut output = Vec::new();
            compositor
                .render(
                    &mut output,
                    &scene,
                    terminal,
                    &caps,
                    &ImageConfig::default(),
                )
                .unwrap();
            let commands = String::from_utf8(output).unwrap();
            assert_eq!(
                displayable_kitty_placements(&commands, &mut uploaded).len(),
                1
            );
            if index > 1 {
                assert!(!commands.contains("a=t"));
            }
        }
        assert_eq!(
            uploaded.len(),
            2,
            "alignment transition uploads at most one source and one fitted raster"
        );
    }

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

    #[cfg(feature = "kitty")]
    #[test]
    fn fractional_letterbox_keeps_identical_raster_when_overlay_clips() {
        let pixels = image::RgbaImage::from_pixel(20, 20, image::Rgba([90, 120, 150, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let mut placed = placement(
            "fractional",
            Rect::new(2, 2, 12, 7),
            Rect::new(0, 0, 80, 24),
            0,
        );
        placed.fit = bmux_tui::image::ImageFit::Contain;
        placed.payload = ImagePayload::Png {
            bytes: bytes.into_inner(),
            width: 20,
            height: 20,
        };
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 9,
            cell_pixel_height: 19,
            ..Default::default()
        };
        let mut compositor = TuiImageCompositor::new();
        let config = ImageConfig::default();
        let mut baseline = None;
        for clip in [
            Rect::new(0, 0, 80, 24),
            Rect::new(2, 2, 6, 7),
            Rect::new(0, 0, 80, 24),
        ] {
            placed.clip = clip;
            let mut scene = ImageScene::default();
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            compositor
                .render(
                    &mut Vec::new(),
                    &scene,
                    PaneRect {
                        x: 0,
                        y: 0,
                        w: 80,
                        h: 24,
                    },
                    &caps,
                    &config,
                )
                .unwrap();
            let fitted = compositor
                .fitted
                .get(&placed.key)
                .expect("sub-cell fit must always use the pixel-centered raster");
            let data = &fitted.1.payload.pixels.as_ref().unwrap().data;
            if let Some(expected) = &baseline {
                assert_eq!(
                    data, expected,
                    "clipping must reuse identical positioned pixels"
                );
            } else {
                baseline = Some(data.clone());
            }
        }
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
                let native = kitty_graphics && compositor.fitted.is_empty();
                let id = if native {
                    assert!(String::from_utf8_lossy(&output).contains("f=100"));
                    assert!(
                        output.len() < bytes.len() * 2 + 4096,
                        "compact upload budget"
                    );
                    compositor.sources.values().next().unwrap().2.id
                } else {
                    let fitted = &compositor.fitted.values().next().unwrap().1;
                    assert_eq!(
                        fitted.payload.pixels.as_ref().unwrap().format,
                        crate::model::PixelFormat::Rgba8
                    );
                    fitted.id
                };
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
                let hidden = ImageScene::default();
                let delta = bmux_tui::image_scene::ImageSceneDelta {
                    removed: vec![ImageKey::new("deck-png")],
                    ..Default::default()
                };
                compositor.apply_delta(&delta);
                compositor
                    .render(
                        &mut Vec::new(),
                        &hidden,
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
                let hidden = scene.clone();
                let mut returned = Vec::new();
                compositor
                    .render(
                        &mut returned,
                        &hidden,
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
                if kitty_graphics {
                    if native {
                        assert_eq!(compositor.sources.values().next().unwrap().2.id, id);
                    }
                    assert!(
                        !String::from_utf8_lossy(&returned).contains("a=t"),
                        "return must only place retained upload"
                    );
                    if native {
                        assert_eq!(compositor.work_counts(), (1, 0, 0));
                    } else {
                        assert_eq!(compositor.fitted.values().next().unwrap().1.id, id);
                    }
                } else {
                    assert_eq!(compositor.fitted.values().next().unwrap().1.id, id);
                }
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

    #[test]
    fn contain_retains_allocation_pixels_across_scroll_and_clip() {
        let mut placed = placement(
            "scroll",
            Rect::new(0, 0, 10, 6),
            Rect::new(0, 0, 20, 20),
            255,
        );
        placed.fit = bmux_tui::image::ImageFit::Contain;
        let config = ImageConfig::default();
        let caps = HostImageCapabilities {
            cell_pixel_width: 4,
            cell_pixel_height: 8,
            ..HostImageCapabilities::default()
        };
        let mut compositor = TuiImageCompositor::new();
        let payload = super::decode_payload(&placed.key, &placed.payload, &config).unwrap();
        let source = crate::frozen::FrozenImage::new(crate::model::PaneImage {
            id: 100,
            protocol: crate::model::ImageProtocol::KittyGraphics,
            position: crate::model::ImagePosition { row: 0, col: 0 },
            cell_size: crate::model::ImageCellSize::default(),
            pixel_size: crate::model::ImagePixelSize {
                width: 8,
                height: 4,
            },
            payload,
        });
        let first = compositor
            .contained_image(&placed, &source, &caps, &config)
            .unwrap();
        let raster = std::sync::Arc::clone(&compositor.rasters[&placed.key].1);
        // Translation changes neither the fitted raster nor the transmitted image.
        placed.destination.y = 3;
        let moved = compositor
            .contained_image(&placed, &source, &caps, &config)
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &moved));
        // The allocation can extend above the terminal: crop it without refitting.
        placed.allocation = Some(bmux_tui::image::ImageBounds {
            x: 0,
            y: -2,
            width: 10,
            height: 6,
        });
        placed.destination = Rect::new(0, 0, 10, 4);
        let clipped = compositor
            .contained_image(&placed, &source, &caps, &config)
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &raster,
            &compositor.rasters[&placed.key].1
        ));
        let expected = super::contain_payload(&placed, &source.payload, &caps, &config).unwrap();
        assert_eq!(clipped.payload, expected);
        // Equal relative crops reuse the protocol resource even after translation.
        placed.allocation.as_mut().unwrap().y += 2;
        placed.destination.y += 2;
        let translated = compositor
            .contained_image(&placed, &source, &caps, &config)
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&clipped, &translated));
        let mut revised = (**source).clone();
        revised.id += 1;
        revised.payload.pixels.as_mut().unwrap().data.fill(0);
        let revised = crate::frozen::FrozenImage::new(revised);
        compositor
            .contained_image(&placed, &revised, &caps, &config)
            .unwrap();
        assert!(!std::sync::Arc::ptr_eq(
            &raster,
            &compositor.rasters[&placed.key].1
        ));
        let updated = std::sync::Arc::clone(&compositor.rasters[&placed.key].1);
        let changed_caps = HostImageCapabilities {
            cell_pixel_height: 10,
            ..caps
        };
        compositor
            .contained_image(&placed, &revised, &changed_caps, &config)
            .unwrap();
        assert!(!std::sync::Arc::ptr_eq(
            &updated,
            &compositor.rasters[&placed.key].1
        ));
        compositor.clear();
        assert!(compositor.rasters.is_empty());
    }

    #[cfg(all(feature = "kitty", feature = "sixel", feature = "iterm2"))]
    #[test]
    fn contain_scroll_reuses_raster_for_every_protocol() {
        for caps in [
            HostImageCapabilities {
                kitty_graphics: true,
                ..Default::default()
            },
            HostImageCapabilities {
                sixel: true,
                ..Default::default()
            },
            HostImageCapabilities {
                iterm2_inline: true,
                ..Default::default()
            },
        ] {
            let caps = HostImageCapabilities {
                cell_pixel_width: 4,
                cell_pixel_height: 8,
                ..caps
            };
            let mut placed = placement(
                "scroll",
                Rect::new(0, 0, 10, 4),
                Rect::new(0, 0, 20, 20),
                255,
            );
            placed.fit = bmux_tui::image::ImageFit::Contain;
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                8,
                4,
                image::Rgba([255; 4]),
            ))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
            placed.payload = ImagePayload::Png {
                bytes: png.into_inner(),
                width: 8,
                height: 4,
            };
            placed.allocation = Some(bmux_tui::image::ImageBounds {
                x: 0,
                y: -2,
                width: 10,
                height: 6,
            });
            let config = ImageConfig::default();
            let terminal = PaneRect {
                x: 0,
                y: 0,
                w: 20,
                h: 20,
            };
            let mut compositor = TuiImageCompositor::new();
            let mut scene = ImageScene::default();
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            let mut output = Vec::new();
            compositor
                .render(&mut output, &scene, terminal, &caps, &config)
                .unwrap();
            assert!(!output.is_empty());
            let raster = std::sync::Arc::clone(&compositor.rasters[&placed.key].1);
            placed.allocation.as_mut().unwrap().y = -1;
            placed.destination.height = 5;
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            output.clear();
            compositor
                .render(&mut output, &scene, terminal, &caps, &config)
                .unwrap();
            assert!(!output.is_empty());
            assert!(std::sync::Arc::ptr_eq(
                &raster,
                &compositor.rasters[&placed.key].1
            ));
            let source = &compositor.sources[&placed.key].2;
            let expected =
                super::contain_payload(&placed, &source.payload, &caps, &config).unwrap();
            let mut actual = compositor.fitted[&placed.key].1.payload.clone();
            if caps.kitty_graphics {
                let bounds = placed.allocation.unwrap();
                super::crop_to_visible(
                    &mut actual,
                    Rect::new(0, 0, bounds.width, bounds.height),
                    super::relative_destination(&placed),
                );
            }
            actual.raw = None;
            assert_eq!(actual, expected);
            assert_eq!(compositor.validations, 1);
        }
    }

    #[test]
    fn contain_raster_eviction_follows_source_budget() {
        let mut compositor = TuiImageCompositor::new();
        let config = ImageConfig::default();
        let caps = HostImageCapabilities::default();
        for i in 0..129 {
            let mut placed = placement(
                &format!("image-{i:03}"),
                Rect::new(0, 0, 2, 2),
                Rect::new(0, 0, 2, 2),
                255,
            );
            placed.fit = bmux_tui::image::ImageFit::Contain;
            let payload = super::decode_payload(&placed.key, &placed.payload, &config).unwrap();
            let source = crate::frozen::FrozenImage::new(crate::model::PaneImage {
                id: i,
                protocol: crate::model::ImageProtocol::KittyGraphics,
                position: crate::model::ImagePosition { row: 0, col: 0 },
                cell_size: crate::model::ImageCellSize::default(),
                pixel_size: crate::model::ImagePixelSize {
                    width: 8,
                    height: 4,
                },
                payload,
            });
            compositor.sources.insert(
                placed.key.clone(),
                (
                    std::sync::Arc::new(placed.payload.clone()),
                    config.max_image_bytes,
                    std::sync::Arc::clone(&source),
                ),
            );
            compositor.source_order.push_back(placed.key.clone());
            compositor
                .contained_image(&placed, &source, &caps, &config)
                .unwrap();
        }
        compositor.trim_sources();
        assert_eq!(compositor.sources.len(), 128);
        assert_eq!(compositor.rasters.len(), 128);
        assert_eq!(compositor.fitted.len(), 128);
        assert!(!compositor.rasters.contains_key(&ImageKey::new("image-000")));
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn native_png_clip_transition_emits_displayable_resources() {
        let mut placed = placement(
            "native-transition",
            Rect::new(0, 2, 10, 6),
            Rect::new(0, 0, 20, 20),
            0,
        );
        placed.fit = bmux_tui::image::ImageFit::Contain;
        placed.payload = transition_png();
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 2,
            cell_pixel_height: 4,
            ..Default::default()
        };
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 20,
            h: 20,
        };
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        let mut uploads = std::collections::BTreeMap::new();
        let mut native_id = None;
        for step in 0..4 {
            match step {
                1 => {
                    placed.allocation = Some(bmux_tui::image::ImageBounds {
                        x: 0,
                        y: -2,
                        width: 10,
                        height: 6,
                    });
                    placed.destination = Rect::new(0, 0, 10, 4);
                }
                2 => {
                    placed.allocation.as_mut().unwrap().y = -1;
                    placed.destination.height = 5;
                }
                3 => {
                    placed.allocation = None;
                    placed.destination = Rect::new(0, 2, 10, 6);
                }
                _ => {}
            }
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            let mut output = Vec::new();
            compositor
                .render(
                    &mut output,
                    &scene,
                    terminal,
                    &caps,
                    &ImageConfig::default(),
                )
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            let placements = displayable_kitty_placements(&output, &mut uploads);
            for placement in &placements {
                let pixels = &uploads[&placement.image_id];
                if step == 0 {
                    native_id = Some(placement.image_id);
                }
                if step == 3 {
                    assert_eq!(Some(placement.image_id), native_id);
                }
                if step == 1 || step == 2 {
                    let source = &compositor.sources[&placed.key].2;
                    let expected = super::contain_payload(
                        &placed,
                        &source.payload,
                        &caps,
                        &ImageConfig::default(),
                    )
                    .unwrap();
                    let crop = placement.source_rect.unwrap();
                    let actual =
                        image::imageops::crop_imm(pixels, crop.x, crop.y, crop.width, crop.height)
                            .to_image();
                    assert_eq!(actual.as_raw(), &expected.pixels.unwrap().data);
                    assert_eq!(placement.cell_size.cols, 10);
                    assert_eq!(placement.cell_size.rows, placed.destination.height);
                }
            }
            assert_eq!(
                placements.len(),
                1,
                "each clip transition must actually place an image"
            );
            if step == 3 {
                assert!(
                    !output.contains("a=t"),
                    "return must reuse native PNG upload"
                );
            }
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn native_clipped_png_scroll_places_source_crop_without_upload() {
        let mut placed = placement("crop", Rect::new(0, 0, 8, 4), Rect::new(0, 0, 20, 20), 0);
        placed.fit = bmux_tui::image::ImageFit::Contain;
        placed.payload = transition_png();
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 1,
            cell_pixel_height: 1,
            ..Default::default()
        };
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 20,
            h: 20,
        };
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        let mut uploads = std::collections::BTreeMap::new();
        let mut host_id = None;
        for (allocation_y, height, source_y) in [(0, 4, 0), (-1, 3, 1), (-2, 2, 2), (0, 4, 0)] {
            placed.allocation = Some(bmux_tui::image::ImageBounds {
                x: 0,
                y: allocation_y,
                width: 8,
                height: 4,
            });
            placed.destination = Rect::new(0, 0, 8, height);
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            let mut output = Vec::new();
            compositor
                .render(
                    &mut output,
                    &scene,
                    terminal,
                    &caps,
                    &ImageConfig::default(),
                )
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            let placements = displayable_kitty_placements(&output, &mut uploads);
            assert_eq!(placements.len(), 1);
            let placement = &placements[0];
            if let Some(id) = host_id {
                assert_eq!(placement.image_id, id);
                assert!(!output.contains("a=t"));
                assert!(
                    output.len() < 1024,
                    "warm clipping must only emit placement commands"
                );
            } else {
                host_id = Some(placement.image_id);
            }
            if source_y != 0 {
                let crop = placement.source_rect.unwrap();
                assert_eq!(
                    (crop.x, crop.y, crop.width, crop.height),
                    (0, source_y, 8, u32::from(height))
                );
                assert_eq!(
                    (placement.cell_size.cols, placement.cell_size.rows),
                    (8, height)
                );
            }
            assert!(
                compositor.rasters.is_empty(),
                "native crop must not CPU-fit pixels"
            );
        }
        assert_eq!(uploads.len(), 1);
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn album_sized_fallback_scroll_retains_full_raster_upload() {
        let mut placed = placement(
            "album-400",
            Rect::new(0, 0, 33, 14),
            Rect::new(0, 0, 80, 40),
            0,
        );
        placed.fit = bmux_tui::image::ImageFit::Contain;
        placed.allocation = Some(bmux_tui::image::ImageBounds {
            x: 0,
            y: -3,
            width: 33,
            height: 17,
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            400,
            400,
            image::Rgba([20, 120, 220, 255]),
        ))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
        placed.payload = ImagePayload::Png {
            bytes: png.into_inner(),
            width: 400,
            height: 400,
        };
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 8,
            cell_pixel_height: 16,
            ..Default::default()
        };
        let terminal = PaneRect {
            x: 0,
            y: 0,
            w: 80,
            h: 40,
        };
        let config = ImageConfig::default();
        let mut compositor = TuiImageCompositor::new();
        let mut scene = ImageScene::default();
        let mut first_id = None;
        for rows in [3_u16, 6, 9, 6, 3] {
            placed.allocation.as_mut().unwrap().y = -i32::from(rows);
            placed.destination.height = 17 - rows;
            scene.reconcile(&[ImageContribution::Present(placed.clone())]);
            let mut output = Vec::new();
            compositor
                .render(&mut output, &scene, terminal, &caps, &config)
                .unwrap();
            let fitted = &compositor.fitted[&placed.key].1;
            if let Some(id) = first_id {
                assert_eq!(id, fitted.id);
                assert!(
                    output.len() < 1024,
                    "warm album clipping must emit <1KiB, got {}",
                    output.len()
                );
                assert!(!String::from_utf8_lossy(&output).contains("a=t"));
            } else {
                first_id = Some(fitted.id);
            }
            let text = String::from_utf8(output).unwrap();
            let crop = text
                .split("\x1b_")
                .filter_map(|body| body.split_once("\x1b\\"))
                .filter_map(|(body, _)| {
                    crate::codec::kitty::parse_command(
                        body.as_bytes(),
                        crate::model::ImagePosition { row: 0, col: 0 },
                    )
                })
                .find_map(|command| {
                    if let crate::model::KittyCommand::Place(p) = command {
                        p.source_rect
                    } else {
                        None
                    }
                })
                .unwrap();
            let full = fitted.payload.pixels.as_ref().unwrap();
            let full =
                image::RgbaImage::from_raw(full.width, full.height, full.data.clone()).unwrap();
            let actual = image::imageops::crop_imm(&full, crop.x, crop.y, crop.width, crop.height)
                .to_image();
            let source = &compositor.sources[&placed.key].2;
            let expected =
                super::contain_payload(&placed, &source.payload, &caps, &config).unwrap();
            assert_eq!(actual.as_raw(), &expected.pixels.unwrap().data);
        }
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn fitted_png_is_lossless_bounded_and_reused() {
        let mut payload = crate::model::ImagePayload {
            raw: None,
            pixels: Some(crate::model::PixelBuffer {
                width: 264,
                height: 272,
                format: crate::model::PixelFormat::Rgba8,
                data: vec![127; 264 * 272 * 4],
            }),
        };
        super::encode_fitted_png(&mut payload);
        let encoded = payload.raw.as_ref().unwrap();
        let pixels = payload.pixels.as_ref().unwrap();
        assert!(encoded.len() < pixels.data.len());
        assert_eq!(
            image::load_from_memory(encoded)
                .unwrap()
                .into_rgba8()
                .as_raw(),
            &pixels.data
        );
        let mut tiny = crate::model::ImagePayload {
            raw: None,
            pixels: Some(crate::model::PixelBuffer {
                width: 1,
                height: 1,
                format: crate::model::PixelFormat::Rgba8,
                data: vec![1, 2, 3, 4],
            }),
        };
        super::encode_fitted_png(&mut tiny);
        assert!(
            tiny.raw.is_none(),
            "oversized wire format must retain RGBA fallback"
        );
    }

    /// Run explicitly with BMUX_COVER_BENCH pointing to a captured decoded cover.
    #[cfg(feature = "kitty")]
    #[test]
    #[ignore = "explicit representative-cover microbenchmark"]
    fn benchmark_real_cover_fitted_png() {
        let path = std::env::var("BMUX_COVER_BENCH").expect("captured decoded cover path");
        let image = image::open(path)
            .unwrap()
            .resize_exact(264, 272, image::imageops::FilterType::Lanczos3)
            .into_rgba8();
        let mut payload = crate::model::ImagePayload {
            raw: None,
            pixels: Some(crate::model::PixelBuffer {
                width: 264,
                height: 272,
                format: crate::model::PixelFormat::Rgba8,
                data: image.into_raw(),
            }),
        };
        for compression in [
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::CompressionType::Default,
        ] {
            let mut times = Vec::new();
            let mut bytes = 0;
            for _ in 0..10 {
                payload.raw = None;
                let started = std::time::Instant::now();
                super::encode_fitted_png_with_compression(&mut payload, compression);
                times.push(started.elapsed().as_micros());
                let encoded = payload
                    .raw
                    .as_ref()
                    .expect("representative cover should compress");
                bytes = encoded.len();
                let decoded = image::load_from_memory(encoded).unwrap().into_rgba8();
                assert_eq!(decoded.as_raw(), &payload.pixels.as_ref().unwrap().data);
            }
            times.sort_unstable();
            eprintln!(
                "fitted PNG compression={compression:?} encode_median_us={} encode_max_us={} rgba_bytes={} png_bytes={bytes} wire_base64_bytes={}",
                times[5],
                times[9],
                payload.pixels.as_ref().unwrap().data.len(),
                bytes.div_ceil(3) * 4
            );
        }
    }

    #[cfg(feature = "kitty")]
    fn transition_png() -> ImagePayload {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            4,
            image::Rgba([20, 120, 220, 255]),
        ))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
        ImagePayload::Png {
            bytes: png.into_inner(),
            width: 8,
            height: 4,
        }
    }

    #[cfg(feature = "kitty")]
    fn displayable_kitty_placements(
        output: &str,
        uploads: &mut std::collections::BTreeMap<u32, image::RgbaImage>,
    ) -> Vec<crate::model::KittyPlacement> {
        use crate::model::{KittyCommand, KittyFormat};
        let mut placements = Vec::new();
        for body in output.split("\x1b_").skip(1) {
            let body = body.split_once("\x1b\\").unwrap().0;
            match crate::codec::kitty::parse_command(
                body.as_bytes(),
                crate::model::ImagePosition { row: 0, col: 0 },
            )
            .unwrap()
            {
                KittyCommand::Transmit {
                    image_id,
                    format,
                    data,
                    width,
                    height,
                    more_chunks,
                } => {
                    assert!(!more_chunks, "small regression images must fit one chunk");
                    let pixels = match format {
                        KittyFormat::Png => image::load_from_memory(&data).unwrap().into_rgba8(),
                        KittyFormat::Rgba => {
                            image::RgbaImage::from_raw(width, height, data).unwrap()
                        }
                        KittyFormat::Rgb => panic!("expected PNG or RGBA"),
                    };
                    uploads.insert(image_id, pixels);
                }
                KittyCommand::Place(placement) => {
                    assert!(
                        uploads[&placement.image_id]
                            .pixels()
                            .any(|p| p.0 == [20, 120, 220, 255]),
                        "placed resource must contain real image pixels"
                    );
                    placements.push(placement);
                }
                KittyCommand::Delete { .. } => {}
                KittyCommand::Query { .. } => panic!("unexpected query"),
            }
        }
        placements
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
