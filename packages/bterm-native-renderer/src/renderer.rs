//! Retained presentation projection into physical quads, not a second layout engine.
use crate::{
    damage::DamageTracker,
    gpu::Gpu,
    images,
    text::{BitmapFormat, RendererGeometry, RendererTypography, TextRasterizer, TextStyle},
};
use bmux_tui::{
    presentation::PreparedPresentation,
    style::{Color, Modifier},
};
use bterm_native_platform::{PhysicalSize, SurfaceTarget};
use std::collections::HashMap;

#[path = "raster_cache.rs"]
pub(crate) mod raster_cache;

#[path = "telemetry.rs"]
pub(crate) mod telemetry;
pub use telemetry::{FrameTelemetry, TelemetrySummary};
use telemetry::{Measurement, Stage, Telemetry};

/// Window-local raster and framebuffer configuration. Sizes are physical except font_size.
#[derive(Clone, Copy, Debug)]
pub struct RendererOptions {
    pub size: PhysicalSize,
    /// Font size in logical pixels.
    pub font_size: f32,
    pub scale_factor: f32,
}
impl Default for RendererOptions {
    fn default() -> Self {
        Self {
            size: PhysicalSize {
                width: 800,
                height: 600,
            },
            font_size: 16.0,
            scale_factor: 1.0,
        }
    }
}
/// Output has been submitted and the acquired surface image presented. Not a scene commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentReport {
    pub generation: u64,
    /// Successful attempt diagnostics only; not scene-commit authority.
    pub telemetry: Option<FrameTelemetry>,
}
/// Explicit failures never produce a successful output report.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("native device: {0}")]
    Device(String),
    #[error("native surface: {0}")]
    Surface(String),
    #[error("native image: {0}")]
    Image(String),
    #[error("native output unsupported: {0}")]
    Unsupported(&'static str),
    #[error("native capacity: {0}")]
    Capacity(&'static str),
    #[error("native text: {0}")]
    Text(#[from] crate::text::TextError),
    #[error("window is minimized")]
    Suspended,
}
/// Window-owned GPU resources and reconstructible raster caches. Drop releases both.
pub struct NativeRenderer {
    gpu: Gpu,
    text: TextRasterizer,
    images: images::ImageCache,
    masks: raster_cache::MaskCache,
    options: RendererOptions,
    typography: RendererTypography,
    cell: [f32; 2],
    ascent: f32,
    damage: DamageTracker,
    telemetry: Option<Box<Telemetry>>,
}
impl NativeRenderer {
    pub async fn new(target: SurfaceTarget, options: RendererOptions) -> Result<Self, RenderError> {
        let typography = RendererTypography {
            font_size: options.font_size,
            ..Default::default()
        };
        let text = TextRasterizer::with_typography(&typography)?;
        let geometry = RendererGeometry::resolve(&text, &typography, options.scale_factor)?;
        let cell = geometry.cell_size;
        let ascent = geometry.baseline;
        let gpu = Gpu::new(target, options.size).await?;
        Ok(Self {
            gpu,
            text,
            images: images::ImageCache::default(),
            masks: raster_cache::MaskCache::default(),
            options,
            typography,
            cell,
            ascent,
            damage: DamageTracker::default(),
            telemetry: None,
        })
    }
    /// Fixed physical cell metrics; execution dimensions are caller-owned.
    pub const fn cell_size(&self) -> [f32; 2] {
        self.cell
    }
    /// Pending geometry, not interaction authority. Snapshot alongside committed output.
    pub const fn geometry(&self) -> RendererGeometry {
        RendererGeometry {
            cell_size: self.cell,
            baseline: self.ascent,
        }
    }
    pub const fn typography(&self) -> &RendererTypography {
        &self.typography
    }
    /// Transactional font/metric replacement. Size animation reuses faces and bounded caches.
    pub fn set_typography(&mut self, typography: RendererTypography) -> Result<(), RenderError> {
        self.set_typography_and_scale(typography, self.options.scale_factor)
    }
    /// Atomically resolve typography and DPI against the requested pair, never an
    /// intermediate combination. On error, faces, options, geometry, caches and
    /// damage remain unchanged. Success updates pending rendering geometry only;
    /// interaction authority still follows successfully committed output.
    pub fn set_typography_and_scale(
        &mut self,
        typography: RendererTypography,
        scale: f32,
    ) -> Result<(), RenderError> {
        typography.validate()?;
        if !scale.is_finite() || scale <= 0.0 {
            return Err(crate::text::TextError::InvalidPixelSize.into());
        }
        if typography == self.typography && scale == self.options.scale_factor {
            return Ok(());
        }
        let replacement = if typography.family != self.typography.family
            || typography.fallback != self.typography.fallback
        {
            Some(TextRasterizer::with_typography(&typography)?)
        } else {
            None
        };
        let text = replacement.as_ref().unwrap_or(&self.text);
        let geometry = RendererGeometry::resolve(text, &typography, scale)?;
        if let Some(text) = replacement {
            self.text = text;
        }
        self.options.font_size = typography.font_size;
        self.options.scale_factor = scale;
        self.typography = typography;
        self.cell = geometry.cell_size;
        self.ascent = geometry.baseline;
        self.damage.invalidate();
        Ok(())
    }
    pub fn resize(&mut self, size: PhysicalSize) {
        self.options.size = size;
        self.gpu.resize(size);
        self.damage.invalidate();
    }
    pub fn set_scale_factor(&mut self, scale: f32) -> Result<(), RenderError> {
        self.set_typography_and_scale(self.typography.clone(), scale)
    }
    /// Enable bounded diagnostics explicitly. Disabled by default; disabling drops
    /// all samples. Re-enabling starts a fresh window. Nothing is logged automatically.
    pub fn set_telemetry_enabled(&mut self, enabled: bool) {
        if enabled && self.telemetry.is_none() {
            self.telemetry = Some(Box::default());
        } else if !enabled {
            self.telemetry = None;
        }
    }
    /// Last renderer attempt, including failures; absent when diagnostics are disabled
    /// or no attempt has run. This is CPU wall-time evidence, never display feedback.
    pub fn last_frame_telemetry(&self) -> Option<FrameTelemetry> {
        self.telemetry
            .as_ref()
            .and_then(|telemetry| telemetry.latest())
    }
    /// Configured `(wgpu PresentMode discriminant, desired maximum frame latency)`.
    /// This reports selected configuration, not measured display refresh or scanout.
    pub fn presentation_config(&self) -> (u32, u32) {
        self.gpu.presentation_config()
    }
    /// Poll lifetime counters and last-128-attempt percentiles, including failures.
    /// Summary calculation is bounded and happens only when requested by the caller.
    pub fn telemetry_summary(&self) -> Option<TelemetrySummary> {
        self.telemetry.as_ref().map(|telemetry| telemetry.summary())
    }
    /// Render complete retained output. Caller must reject stale provenance before entry
    /// and commit interaction metadata only if this method succeeds.
    pub fn present(
        &mut self,
        prepared: &PreparedPresentation,
    ) -> Result<PresentReport, RenderError> {
        let enabled = self.telemetry.is_some();
        let mut measurement = Measurement::new(enabled);
        self.gpu.begin_telemetry(enabled);
        let mut result = self.present_inner(prepared, &mut measurement);
        if let Some(telemetry) = &mut self.telemetry {
            let sample = measurement.finish(self.gpu.frame_telemetry());
            telemetry.record(sample, result.is_err());
            if let Ok(report) = &mut result {
                report.telemetry = Some(sample);
            }
        }
        result
    }
    fn draw_image(
        &mut self,
        placement: &bmux_tui::image::ImagePlacement,
        origin: [f32; 2],
        cell: [f32; 2],
        winning_clip: [f32; 4],
        measurement: &mut Measurement,
    ) -> Result<(), RenderError> {
        let Some((rect, clip)) = image_geometry(placement, origin, cell, winning_clip) else {
            return Ok(());
        };
        let key = images::cache_key(&placement.payload)?;
        let (image, cache_hit) = self.images.get_with_status(key)?;
        if cache_hit {
            measurement.sample.image_cache_hits += 1;
        } else {
            measurement.sample.image_cache_misses += 1;
            measurement.sample.image_decode_count += 1;
        }
        let uv = self
            .gpu
            .upload_cached(image.width, image.height, &image.rgba)?;
        self.gpu.quad(rect, Some(uv), [1.0; 4], clip)
    }

    fn present_inner(
        &mut self,
        prepared: &PreparedPresentation,
        measurement: &mut Measurement,
    ) -> Result<PresentReport, RenderError> {
        if self.options.size.is_empty() {
            return Err(RenderError::Suspended);
        }
        if prepared.buffer().cells().len() > 1_000_000
            || prepared.image_scene().placements().len() > 4096
        {
            return Err(RenderError::Capacity("presentation item budget exhausted"));
        }
        let base_size = self.options.font_size * self.options.scale_factor;
        for fragment in prepared.grid_fragments() {
            validate_fragment_raster_size(base_size, fragment.transform().scale() as f32)?;
        }
        let image_count = prepared
            .grid_fragments()
            .iter()
            .try_fold(
                prepared.image_scene().placements().len(),
                |count, fragment| count.checked_add(fragment.images().len()),
            )
            .ok_or(RenderError::Capacity("fragment image count overflow"))?;
        let image_work = prepared
            .grid_fragments()
            .iter()
            .try_fold(0_usize, |count, fragment| {
                count.checked_add(
                    fragment
                        .images()
                        .len()
                        .saturating_mul(fragment.visible_clips().len()),
                )
            })
            .ok_or(RenderError::Capacity("fragment image work overflow"))?;
        if image_count > 4096 || image_work > 1_000_000 {
            return Err(RenderError::Capacity("fragment image budget exhausted"));
        }
        let clip_count: usize = prepared
            .grid_fragments()
            .iter()
            .map(|fragment| fragment.visible_clips().len())
            .sum();
        if clip_count.saturating_mul(prepared.image_scene().placements().len()) > 1_000_000 {
            return Err(RenderError::Capacity(
                "fragment image visibility budget exhausted",
            ));
        }
        if prepared.grid_fragments().iter().any(|fragment| {
            fragment.visible_clips().iter().any(|clip| {
                prepared
                    .image_scene()
                    .placements()
                    .any(|image| !image.destination.intersection(*clip).is_empty())
                    || prepared
                        .cursor()
                        .is_some_and(|cursor| cursor.visible && clip.contains(cursor.position))
            })
        }) {
            return Err(RenderError::Unsupported(
                "root images/cursor overlap a retained local grid",
            ));
        }
        let fragment_cells = prepared
            .grid_fragments()
            .iter()
            .try_fold(0_usize, |total, fragment| {
                total.checked_add(
                    fragment
                        .buffer()
                        .cells()
                        .len()
                        .saturating_mul(fragment.visible_clips().len()),
                )
            })
            .ok_or(RenderError::Capacity("fragment work budget exhausted"))?;
        if fragment_cells > 1_000_000 {
            return Err(RenderError::Capacity("fragment work budget exhausted"));
        }
        let overlay_work = prepared
            .grid_fragments()
            .iter()
            .try_fold(0_usize, |total, fragment| {
                let count = fragment.overlays().len();
                let cells = fragment
                    .overlays()
                    .iter()
                    .fold(fragment.buffer().cells().len(), |n, overlay| {
                        n.saturating_add(overlay.buffer().cells().len())
                    });
                total.checked_add(
                    cells
                        .saturating_mul(fragment.visible_clips().len())
                        .saturating_mul(count.saturating_add(1).saturating_pow(2)),
                )
            })
            .ok_or(RenderError::Capacity("overlay work overflow"))?;
        if overlay_work > 1_000_000 {
            return Err(RenderError::Capacity("overlay work budget exhausted"));
        }
        self.gpu.begin();
        let mut overlay_clip_budget = 1_000_000_usize;
        let [cw, ch] = self.cell;
        let screen = [
            0.0,
            0.0,
            self.options.size.width as f32,
            self.options.size.height as f32,
        ];
        // Final compositor-winning clips exclude higher root chrome/overlays.
        // This iterator is output only, never an independent scene authority.
        let root = std::iter::once((
            prepared.buffer(),
            [0.0, 0.0],
            1.0_f32,
            screen,
            None,
            &[][..],
            None,
            &[][..],
        ));
        let fragments = prepared.grid_fragments().iter().flat_map(|fragment| {
            let transform = fragment.transform();
            let origin = transform.origin();
            fragment.visible_clips().iter().flat_map(move |clip| {
                let visible = [
                    f32::from(clip.x) * cw,
                    f32::from(clip.y) * ch,
                    f32::from(clip.width) * cw,
                    f32::from(clip.height) * ch,
                ];
                std::iter::once((
                    fragment.buffer(),
                    [f32::from(origin.x) * cw, f32::from(origin.y) * ch],
                    transform.scale() as f32,
                    visible,
                    fragment.cursor(),
                    fragment.images(),
                    None,
                    fragment.overlays(),
                ))
                .chain(fragment.overlays().iter().enumerate().filter_map(
                    move |(index, overlay)| {
                        let rect = overlay.clip();
                        let clip = intersect_clip(
                            visible,
                            [
                                f32::from(rect.x) * cw,
                                f32::from(rect.y) * ch,
                                f32::from(rect.width) * cw,
                                f32::from(rect.height) * ch,
                            ],
                        )?;
                        Some((
                            overlay.buffer(),
                            [0.0, 0.0],
                            1.0,
                            clip,
                            None,
                            &[][..],
                            Some(overlay.paint_background()),
                            &fragment.overlays()[index + 1..],
                        ))
                    },
                ))
            })
        });
        for (buffer, origin, scale, visible_clip, cursor, local_images, sparse_background, later) in
            root.chain(fragments)
        {
            let area = buffer.area();
            let [cw, ch] = [cw * scale, ch * scale];
            // Swapchain images are not persistent; every accepted grid is redrawn.
            for (index, cell) in buffer.cells().iter().enumerate() {
                let x = f32::from(area.x) + (index % usize::from(area.width)) as f32;
                let y = f32::from(area.y) + (index / usize::from(area.width)) as f32;
                if sparse_background == Some(false) {
                    continue;
                }
                let (_, bg) = colors(cell.style.fg, cell.style.bg, cell.style.modifiers);
                self.gpu.quad(
                    [origin[0] + x * cw, origin[1] + y * ch, cw, ch],
                    None,
                    bg,
                    visible_clip,
                )?;
            }
            // Deduplicate repeated cells within the frame; GPU retention owns atlas lifetime.
            measurement.stage(Some(Stage::Text));
            let mut uploaded = HashMap::new();
            for (index, cell) in buffer.cells().iter().enumerate() {
                if cell.is_wide_continuation() || cell.style.modifiers.contains(Modifier::HIDDEN) {
                    continue;
                }
                let x =
                    origin[0] + (f32::from(area.x) + (index % usize::from(area.width)) as f32) * cw;
                let y =
                    origin[1] + (f32::from(area.y) + (index / usize::from(area.width)) as f32) * ch;
                let cell_width = cw * f32::from(cell.width());
                let Some(clip) = intersect_clip([x, y, cell_width, ch], visible_clip) else {
                    continue;
                };
                // The empty-overlay (including uniform-root) path allocates no
                // clip vector and keeps its original single raster pass.
                let uncovered = if later.is_empty() {
                    Vec::new()
                } else {
                    uncovered_overlay_clips(clip, later, self.cell, &mut overlay_clip_budget)?
                };
                for clip in later
                    .is_empty()
                    .then_some(clip)
                    .into_iter()
                    .chain(uncovered)
                {
                    let (fg, _) = colors(cell.style.fg, cell.style.bg, cell.style.modifiers);
                    let style = TextStyle {
                        bold: cell.style.modifiers.contains(Modifier::BOLD),
                        italic: cell.style.modifiers.contains(Modifier::ITALIC),
                    };
                    let (baseline, phase) =
                        crate::text::raster_origin([x, y + self.ascent * scale]);
                    let raster = self.text.rasterize_cell_phased(
                        &cell.symbol,
                        u16::from(cell.width()),
                        style,
                        self.options.font_size * self.options.scale_factor * scale,
                        phase,
                    )?;
                    for (glyph_index, glyph) in raster.glyphs.iter().enumerate() {
                        if glyph.width == 0 || glyph.height == 0 {
                            continue;
                        }
                        let key = (
                            cell.symbol.as_str(),
                            cell.width(),
                            style.bold,
                            style.italic,
                            phase,
                            glyph_index,
                        );
                        let uv = if let Some(uv) = uploaded.get(&key) {
                            *uv
                        } else {
                            let uv = match glyph.format {
                                BitmapFormat::Rgba => self.gpu.upload_cached(
                                    glyph.width,
                                    glyph.height,
                                    &glyph.pixels,
                                )?,
                                BitmapFormat::Mask => {
                                    let (rgba, cache_hit) = self.masks.get_with_status(
                                        glyph.width,
                                        glyph.height,
                                        &glyph.pixels,
                                    )?;
                                    if cache_hit {
                                        measurement.sample.glyph_rgba_cache_hits += 1;
                                    } else {
                                        measurement.sample.glyph_rgba_cache_misses += 1;
                                    }
                                    self.gpu.upload_cached(glyph.width, glyph.height, &rgba)?
                                }
                            };
                            uploaded.insert(key, uv);
                            uv
                        };
                        let tint = if glyph.format == BitmapFormat::Rgba {
                            [1.0; 4]
                        } else {
                            fg
                        };
                        self.gpu.quad(
                            [
                                baseline[0] + glyph.x + glyph.left as f32,
                                baseline[1] + glyph.y - glyph.top as f32,
                                glyph.width as f32,
                                glyph.height as f32,
                            ],
                            Some(uv),
                            tint,
                            clip,
                        )?;
                    }
                    if cell.style.modifiers.contains(Modifier::UNDERLINE) {
                        self.gpu.quad(
                            [x, y + ch - 2.0 * scale, cell_width, scale],
                            None,
                            fg,
                            clip,
                        )?;
                    }
                    if cell.style.modifiers.contains(Modifier::CROSSED_OUT) {
                        self.gpu
                            .quad([x, y + ch * 0.5, cell_width, scale], None, fg, clip)?;
                    }
                }
            }
            measurement.stage(Some(Stage::Image));
            for placement in local_images {
                self.draw_image(placement, origin, [cw, ch], visible_clip, measurement)?;
            }
            if let Some(cursor) = cursor.filter(|cursor| cursor.visible) {
                for clip in uncovered_overlay_clips(
                    visible_clip,
                    later,
                    self.cell,
                    &mut overlay_clip_budget,
                )? {
                    self.gpu.quad(
                        [
                            origin[0] + f32::from(cursor.position.x) * cw,
                            origin[1] + f32::from(cursor.position.y) * ch + ch - 2.0 * scale,
                            cw,
                            2.0 * scale,
                        ],
                        None,
                        [1.0; 4],
                        clip,
                    )?;
                }
            }
        }
        measurement.stage(Some(Stage::Image));
        // Borrowed full-content keys deduplicate placements even when a decoded image
        // exceeds the retention budget. No payload or decoded pixels are retained here.
        let mut uploaded_images = HashMap::new();
        for placement in prepared.image_scene().placements() {
            let key = images::cache_key(&placement.payload)?;
            let uv = match uploaded_images.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let (image, cache_hit) = self.images.get_with_status(key)?;
                    if cache_hit {
                        measurement.sample.image_cache_hits += 1;
                    } else {
                        measurement.sample.image_cache_misses += 1;
                        measurement.sample.image_decode_count += 1;
                    }
                    let uv = self
                        .gpu
                        .upload_cached(image.width, image.height, &image.rgba)?;
                    entry.insert(uv);
                    uv
                }
            };
            let rect = placement.pixel_rect(f64::from(cw), f64::from(ch));
            let clip = placement.clip.intersection(placement.destination);
            self.gpu.quad(
                [
                    rect.x as f32,
                    rect.y as f32,
                    rect.width as f32,
                    rect.height as f32,
                ],
                Some(uv),
                [1.0; 4],
                [
                    f32::from(clip.x) * cw,
                    f32::from(clip.y) * ch,
                    f32::from(clip.width) * cw,
                    f32::from(clip.height) * ch,
                ],
            )?;
        }
        measurement.stage(None);
        if let Some(cursor) = prepared.cursor().filter(|cursor| cursor.visible) {
            self.gpu.quad(
                [
                    f32::from(cursor.position.x) * cw,
                    f32::from(cursor.position.y) * ch + ch - 2.0,
                    cw,
                    2.0,
                ],
                None,
                [1.0; 4],
                screen,
            )?;
        }
        self.gpu.present()?;
        Ok(PresentReport {
            generation: self.damage.committed(),
            telemetry: None,
        })
    }
}
/// Subtract later original replacement spans from foreground/cursor paint only.
/// Backgrounds and images remain underneath transparent foregrounds.
fn uncovered_overlay_clips(
    clip: [f32; 4],
    overlays: &[bmux_tui::presentation::PreparedGridOverlay],
    cell: [f32; 2],
    budget: &mut usize,
) -> Result<Vec<[f32; 4]>, RenderError> {
    let mut clips = vec![clip];
    for overlay in overlays {
        let rect = overlay.clip();
        let cut = [
            f32::from(rect.x) * cell[0],
            f32::from(rect.y) * cell[1],
            f32::from(rect.width) * cell[0],
            f32::from(rect.height) * cell[1],
        ];
        let mut next = Vec::new();
        for rect in clips {
            *budget = budget
                .checked_sub(1)
                .ok_or(RenderError::Capacity("overlay clip work exhausted"))?;
            if let Some(hit) = intersect_clip(rect, cut) {
                for piece in [
                    [rect[0], rect[1], rect[2], hit[1] - rect[1]],
                    [
                        rect[0],
                        hit[1] + hit[3],
                        rect[2],
                        rect[1] + rect[3] - hit[1] - hit[3],
                    ],
                    [rect[0], hit[1], hit[0] - rect[0], hit[3]],
                    [
                        hit[0] + hit[2],
                        hit[1],
                        rect[0] + rect[2] - hit[0] - hit[2],
                        hit[3],
                    ],
                ] {
                    if piece[2] > 0.0 && piece[3] > 0.0 {
                        next.push(piece);
                    }
                }
            } else {
                next.push(rect);
            }
            if next.len() > 4096 {
                return Err(RenderError::Capacity("overlay clip count exhausted"));
            }
        }
        clips = next;
    }
    Ok(clips)
}

fn validate_fragment_raster_size(base: f32, scale: f32) -> Result<f32, RenderError> {
    let effective = base * scale;
    if !effective.is_finite()
        || !(crate::text::MIN_RASTER_PIXEL_SIZE..=crate::text::MAX_RASTER_PIXEL_SIZE)
            .contains(&effective)
    {
        return Err(RenderError::Text(crate::text::TextError::InvalidPixelSize));
    }
    Ok(effective)
}

fn image_geometry(
    placement: &bmux_tui::image::ImagePlacement,
    origin: [f32; 2],
    cell: [f32; 2],
    winning_clip: [f32; 4],
) -> Option<([f32; 4], [f32; 4])> {
    let fitted = placement.pixel_rect(f64::from(cell[0]), f64::from(cell[1]));
    let rect = [
        origin[0] + fitted.x as f32,
        origin[1] + fitted.y as f32,
        fitted.width as f32,
        fitted.height as f32,
    ];
    let local_clip = placement.clip.intersection(placement.destination);
    let clip = intersect_clip(
        [
            origin[0] + f32::from(local_clip.x) * cell[0],
            origin[1] + f32::from(local_clip.y) * cell[1],
            f32::from(local_clip.width) * cell[0],
            f32::from(local_clip.height) * cell[1],
        ],
        winning_clip,
    )?;
    intersect_clip(rect, clip)?;
    Some((rect, clip))
}

fn intersect_clip(a: [f32; 4], b: [f32; 4]) -> Option<[f32; 4]> {
    let x = a[0].max(b[0]);
    let y = a[1].max(b[1]);
    let right = (a[0] + a[2]).min(b[0] + b[2]);
    let bottom = (a[1] + a[3]).min(b[1] + b[3]);
    (right > x && bottom > y).then_some([x, y, right - x, bottom - y])
}

fn colors(fg: Option<Color>, bg: Option<Color>, modifiers: Modifier) -> ([f32; 4], [f32; 4]) {
    let mut fg = color(fg.unwrap_or_default(), [220, 220, 220]);
    let mut bg = color(bg.unwrap_or_default(), [16, 16, 16]);
    if modifiers.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if modifiers.contains(Modifier::DIM) {
        for value in &mut fg[..3] {
            *value *= 0.5;
        }
    }
    (fg, bg)
}
fn color(color: Color, default: [u8; 3]) -> [f32; 4] {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [229, 229, 229],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [255, 255, 255],
    ];
    let rgb = match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(i) => match i {
            0..=15 => ANSI[usize::from(i)],
            16..=231 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
                [level(i / 36), level(i / 6 % 6), level(i % 6)]
            }
            _ => [8 + 10 * (i - 232); 3],
        },
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::White => ANSI[7],
        Color::BrightBlack => ANSI[8],
        Color::BrightRed => ANSI[9],
        Color::BrightGreen => ANSI[10],
        Color::BrightYellow => ANSI[11],
        Color::BrightBlue => ANSI[12],
        Color::BrightMagenta => ANSI[13],
        Color::BrightCyan => ANSI[14],
        Color::BrightWhite => ANSI[15],
    };
    // Vertex colors enter an sRGB framebuffer as linear values.
    let linear = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    [linear(rgb[0]), linear(rgb[1]), linear(rgb[2]), 1.0]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_image_fit_and_fractional_overlay_clipping_share_metrics() {
        use bmux_tui::image::{
            ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
        };
        let placement = ImagePlacement {
            key: ImageKey::new("local"),
            payload: ImagePayload::Pixels {
                bytes: vec![255; 16],
                width: 2,
                height: 2,
                format: ImagePixelFormat::Rgba8,
            },
            destination: bmux_tui::geometry::Rect::new(0, 0, 4, 2),
            allocation: None,
            clip: bmux_tui::geometry::Rect::new(0, 0, 4, 2),
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Frame,
        };
        for scale in [0.875_f32, 1.375] {
            let cell = [8.0 * scale, 16.0 * scale];
            let (rect, clip) =
                image_geometry(&placement, [16.0, 32.0], cell, [16.0, 32.0, 100.0, 10.0]).unwrap();
            assert_eq!(rect, [16.0, 32.0, 32.0 * scale, 32.0 * scale]);
            assert_eq!(clip[3], 10.0);
            assert_eq!(clip[0], rect[0]);
            assert!(image_geometry(&placement, [16.0, 32.0], cell, [0.0, 0.0, 1.0, 1.0]).is_none());
        }
        assert_eq!(validate_fragment_raster_size(128.0, 2.0).unwrap(), 256.0);
        assert!(validate_fragment_raster_size(128.0, 2.001).is_err());
        assert!(validate_fragment_raster_size(16.0, f32::NAN).is_err());
    }

    #[test]
    fn transparent_replacement_removes_lower_glyph_not_background_or_image() {
        use bmux_tui::{buffer::Buffer, geometry::Rect, presentation::PreparedGridOverlay};
        use std::sync::Arc;
        let overlay = PreparedGridOverlay::new(
            Arc::new(Buffer::empty(Rect::new(1, 0, 1, 1))),
            Rect::new(1, 0, 1, 1),
            false,
        )
        .unwrap();
        assert!(!overlay.paint_background());
        assert!(
            uncovered_overlay_clips(
                [0.0, 0.0, 30.0, 20.0],
                std::slice::from_ref(&overlay),
                [10.0, 20.0],
                &mut 0
            )
            .is_err()
        );
        let clips = uncovered_overlay_clips(
            [0.0, 0.0, 30.0, 20.0],
            &[overlay],
            [10.0, 20.0],
            &mut 1_000_000,
        )
        .unwrap();
        // The same subtraction is used for the local cursor; local images are
        // deliberately not subtracted, so their pixels show behind sparse text.
        assert_eq!(clips, vec![[0.0, 0.0, 10.0, 20.0], [20.0, 0.0, 10.0, 20.0]]);
    }

    #[test]
    fn original_spans_obey_canonical_later_wins_and_fractional_wide_clip() {
        use bmux_tui::{buffer::Buffer, geometry::Rect, presentation::PreparedGridOverlay};
        use std::sync::Arc;
        let overlays = [Rect::new(1, 0, 1, 1), Rect::new(2, 0, 1, 1)].map(|rect| {
            PreparedGridOverlay::new(Arc::new(Buffer::empty(rect)), rect, false).unwrap()
        });
        let clips = uncovered_overlay_clips(
            [5.0, 0.0, 30.0, 20.0],
            &overlays,
            [10.0, 20.0],
            &mut 1_000_000,
        )
        .unwrap();
        assert_eq!(clips, vec![[5.0, 0.0, 5.0, 20.0], [30.0, 0.0, 5.0, 20.0]]);
        assert!(
            uncovered_overlay_clips(
                [10.0, 0.0, 10.0, 20.0],
                &overlays,
                [10.0, 20.0],
                &mut 1_000_000
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn fragment_clip_excludes_overlay_and_bounds_wide_cells() {
        assert_eq!(
            intersect_clip([0.0, 0.0, 20.0, 10.0], [5.0, 0.0, 5.0, 10.0]),
            Some([5.0, 0.0, 5.0, 10.0])
        );
        assert_eq!(
            intersect_clip([0.0, 0.0, 5.0, 10.0], [5.0, 0.0, 5.0, 10.0]),
            None
        );
    }
    #[test]
    fn indexed_endpoints_and_reverse_are_exact() {
        assert_eq!(color(Color::Indexed(16), [1; 3]), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(color(Color::Indexed(231), [1; 3]), [1.0; 4]);
        let normal = colors(Some(Color::Red), Some(Color::Blue), Modifier::EMPTY);
        let reverse = colors(Some(Color::Red), Some(Color::Blue), Modifier::REVERSED);
        assert_eq!(normal, (reverse.1, reverse.0));
    }
}
