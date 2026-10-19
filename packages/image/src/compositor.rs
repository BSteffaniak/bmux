//! Image compositor — renders images as a post-render overlay.
//!
//! Called after the cell-based `render_attach_scene` and before cursor
//! state application.  Translates pane-local image coordinates to host
//! terminal coordinates, clips to pane boundaries, and emits the
//! appropriate protocol-specific escape sequences.

mod cache;
pub(crate) mod clipping;
#[cfg(all(test, any(feature = "kitty", feature = "sixel")))]
mod tests;
use std::io::Write;

#[cfg(feature = "iterm2")]
use image::ImageEncoder;

use crate::config::ImageDecodeMode;
use crate::host_caps::HostImageCapabilities;
use crate::model::PaneImage;

/// Rectangle describing a pane's position on the host terminal.
#[derive(Clone, Copy, Debug)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Tracks kitty images already transmitted to the host terminal.
/// This enables transmit-once-place-many: only new images are transmitted,
/// previously transmitted images are re-placed without re-sending data.
#[derive(Clone, Debug, Default)]
pub struct KittyHostState {
    /// Maps bmux-internal image ID → host-side kitty image ID.
    pub transmitted: std::collections::HashMap<u64, u32>,
    /// Next host-side kitty image ID to allocate.
    #[cfg(feature = "kitty")]
    next_host_id: u32,
    cache: cache::Cache,
    protocol: Option<crate::model::ImageProtocol>,
    #[cfg(feature = "kitty")]
    content_bytes: std::collections::HashMap<u64, usize>,
    frame_seen: std::collections::HashSet<u64>,
    // Retained resources needed later in this frame, before their paint turn.
    frame_protected: std::collections::HashSet<u64>,
    repair_ids: std::collections::BTreeSet<u32>,
    allocated_ids: std::collections::BTreeSet<u32>,
    #[cfg(feature = "kitty")]
    placements: std::collections::BTreeMap<(u32, u16, u16), u32>,
    #[cfg(feature = "kitty")]
    seen_placements: std::collections::BTreeSet<(u32, u16, u16)>,
    #[cfg(feature = "kitty")]
    next_placement_id: u32,
}

impl KittyHostState {
    /// Place an already-validated PNG with protocol-native aspect-preserving scaling.
    /// One cell dimension is deliberately omitted so Kitty derives it from source
    /// aspect ratio. No CPU resize, letterbox raster, or decoded upload is needed.
    #[cfg(feature = "kitty")]
    pub(crate) fn contain_png(
        &mut self,
        out: &mut impl Write,
        image: &PaneImage,
        placement: &bmux_tui::image::ImagePlacement,
        caps: &HostImageCapabilities,
        clip: bmux_tui::geometry::Rect,
    ) -> std::io::Result<bool> {
        use bmux_tui::geometry::Rect;
        let bounds = placement
            .allocation
            .unwrap_or_else(|| placement.destination.into());
        let (cw, ch) = caps.fitting_cell_size();
        let fit = placement.pixel_rect(f64::from(cw), f64::from(ch));
        // Native PNG placement centers at cell boundaries, whereas the fitted
        // raster centers in pixels. Use the raster for sub-cell letterboxing
        // even before clipping, so opening an overlay cannot move the image.
        if [
            fit.x / f64::from(cw),
            fit.y / f64::from(ch),
            fit.width / f64::from(cw),
            fit.height / f64::from(ch),
        ]
        .into_iter()
        .any(|value| !value.is_finite() || (value - value.round()).abs() > 0.000001)
        {
            return Ok(false);
        }
        if bounds.x < 0 || bounds.y < 0 {
            return self.contain_png_cropped(out, image, placement, caps, clip);
        }
        let (Ok(x), Ok(y)) = (u16::try_from(bounds.x), u16::try_from(bounds.y)) else {
            return Ok(false);
        };
        let allocation = Rect::new(x, y, bounds.width, bounds.height);
        if allocation.intersection(clip) != allocation {
            return self.contain_png_cropped(out, image, placement, caps, clip);
        }
        let Some(pixels) = image
            .payload
            .pixels
            .as_ref()
            .filter(|p| p.format == crate::model::PixelFormat::Png)
        else {
            return Ok(false);
        };
        let (cw, ch) = caps.fitting_cell_size();
        let width_limited = u64::from(allocation.width) * u64::from(cw) * u64::from(pixels.height)
            <= u64::from(allocation.height) * u64::from(ch) * u64::from(pixels.width);
        let (cols, rows, footprint_width, footprint_height) = if width_limited {
            let height = (u64::from(allocation.width) * u64::from(cw) * u64::from(pixels.height))
                .div_ceil(u64::from(pixels.width) * u64::from(ch));
            (
                allocation.width,
                0,
                allocation.width,
                u16::try_from(height).unwrap_or(allocation.height),
            )
        } else {
            let width = (u64::from(allocation.height) * u64::from(ch) * u64::from(pixels.width))
                .div_ceil(u64::from(pixels.height) * u64::from(cw));
            (
                0,
                allocation.height,
                u16::try_from(width).unwrap_or(allocation.width),
                allocation.height,
            )
        };
        let x = x + allocation.width.saturating_sub(footprint_width) / 2;
        let y = y + allocation.height.saturating_sub(footprint_height) / 2;
        let fragment = PaneImage {
            cell_size: crate::model::ImageCellSize { cols, rows },
            ..image.clone()
        };
        write!(out, "\x1b[{};{}H", y + 1, x + 1)?;
        emit_kitty_data(
            out,
            &fragment,
            crate::model::KittyFormat::Png,
            &pixels.data,
            pixels.width,
            pixels.height,
            x,
            y,
            self,
        )?;
        Ok(true)
    }

    #[cfg(feature = "kitty")]
    fn contain_png_cropped(
        &mut self,
        out: &mut impl Write,
        image: &PaneImage,
        placement: &bmux_tui::image::ImagePlacement,
        caps: &HostImageCapabilities,
        clip: bmux_tui::geometry::Rect,
    ) -> std::io::Result<bool> {
        let Some(pixels) = image
            .payload
            .pixels
            .as_ref()
            .filter(|p| p.format == crate::model::PixelFormat::Png)
        else {
            return Ok(false);
        };
        let (cw, ch) = caps.fitting_cell_size();
        let fit = placement.pixel_rect(f64::from(cw), f64::from(ch));
        let visible = placement.destination.intersection(clip);
        let left = fit.x.max(f64::from(visible.x) * f64::from(cw));
        let top = fit.y.max(f64::from(visible.y) * f64::from(ch));
        let right = (fit.x + fit.width).min(f64::from(visible.right()) * f64::from(cw));
        let bottom = (fit.y + fit.height).min(f64::from(visible.bottom()) * f64::from(ch));
        if right <= left || bottom <= top {
            return Ok(true);
        }
        // Kitty's source crop and cell extent are integral. Only take this path
        // when all boundaries are exactly representable, never stretch a rounded crop.
        let values = [
            left / f64::from(cw),
            top / f64::from(ch),
            (right - left) / f64::from(cw),
            (bottom - top) / f64::from(ch),
            (left - fit.x) * f64::from(pixels.width) / fit.width,
            (top - fit.y) * f64::from(pixels.height) / fit.height,
            (right - left) * f64::from(pixels.width) / fit.width,
            (bottom - top) * f64::from(pixels.height) / fit.height,
        ];
        let mut geometry = Vec::with_capacity(8);
        for value in values {
            if !value.is_finite()
                || value < 0.0
                || value > f64::from(u32::MAX)
                || (value - value.round()).abs() > 0.000001
            {
                return Ok(false);
            }
            // Parsing avoids lossy float casts; the exact-integral check above is
            // also the correctness gate for terminal-side aspect-preserving fitting.
            geometry.push(
                format!("{value:.0}")
                    .parse::<u32>()
                    .expect("bounded integer"),
            );
        }
        let (Ok(x), Ok(y), Ok(cols), Ok(rows)) = (
            u16::try_from(geometry[0]),
            u16::try_from(geometry[1]),
            u16::try_from(geometry[2]),
            u16::try_from(geometry[3]),
        ) else {
            return Ok(false);
        };
        let host = upload_kitty_data(
            out,
            image,
            crate::model::KittyFormat::Png,
            &pixels.data,
            pixels.width,
            pixels.height,
            self,
        )?;
        write!(out, "\x1b[{};{}H", y + 1, x + 1)?;
        let crop = crate::model::KittySourceRect {
            x: geometry[4],
            y: geometry[5],
            width: geometry[6],
            height: geometry[7],
        };
        self.place_cropped(out, host, (x, y), (cols, rows), Some(crop))?;
        Ok(true)
    }

    /// Place a retained, allocation-sized raster with an exact output-pixel crop.
    /// No resampling is needed: every cell boundary maps to an integer raster pixel.
    #[cfg(feature = "kitty")]
    pub(crate) fn contained_raster(
        &mut self,
        out: &mut impl Write,
        image: &PaneImage,
        placement: &bmux_tui::image::ImagePlacement,
        caps: &HostImageCapabilities,
        clip: bmux_tui::geometry::Rect,
    ) -> std::io::Result<()> {
        let visible = placement.destination.intersection(clip);
        if visible.is_empty() {
            return Ok(());
        }
        let bounds = placement
            .allocation
            .unwrap_or_else(|| placement.destination.into());
        let (cw, ch) = caps.fitting_cell_size();
        let pixels = image.payload.pixels.as_ref().expect("contained raster");
        let source_x = u32::try_from(i64::from(visible.x) - i64::from(bounds.x))
            .expect("validated allocation")
            * u32::from(cw);
        let source_y = u32::try_from(i64::from(visible.y) - i64::from(bounds.y))
            .expect("validated allocation")
            * u32::from(ch);
        let crop = crate::model::KittySourceRect {
            x: source_x,
            y: source_y,
            width: u32::from(visible.width) * u32::from(cw),
            height: u32::from(visible.height) * u32::from(ch),
        };
        // Adapter-owned fitted images retain optional lossless PNG wire bytes
        // alongside RGBA pixels so cropping and non-Kitty fallback stay identical.
        let (format, data) = image.payload.raw.as_ref().map_or(
            (crate::model::KittyFormat::Rgba, pixels.data.as_slice()),
            |png| (crate::model::KittyFormat::Png, png.as_slice()),
        );
        let host = upload_kitty_data(out, image, format, data, pixels.width, pixels.height, self)?;
        write!(out, "\x1b[{};{}H", visible.y + 1, visible.x + 1)?;
        self.place_cropped(
            out,
            host,
            (visible.x, visible.y),
            (visible.width, visible.height),
            Some(crop),
        )
    }

    /// Hide all compositor-owned Kitty placements while retaining reusable content.
    /// The caller must commit this candidate only after output and flush succeed.
    pub fn clear_placements(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        #[cfg(feature = "kitty")]
        for host_id in self.transmitted.values() {
            out.write_all(b"\x1b_")?;
            out.write_all(&crate::codec::kitty::encode_delete_image(*host_id))?;
            out.write_all(b"\x1b\\")?;
        }
        #[cfg(not(feature = "kitty"))]
        let _ = out;
        #[cfg(feature = "kitty")]
        self.placements.clear();
        Ok(())
    }

    /// Start one complete host frame (all panes), on a cloned candidate state.
    pub fn begin_frame(&mut self) {
        self.frame_seen.clear();
        self.frame_protected.clear();
        self.allocated_ids.clear();
        #[cfg(feature = "kitty")]
        self.seen_placements.clear();
    }

    /// Protect an already resident resource before ordered painting begins.
    /// This does not count as a placement or allocate host content.
    pub(crate) fn protect_retained(&mut self, id: u64) {
        if self.transmitted.contains_key(&id) {
            self.frame_protected.insert(id);
        }
    }

    /// Invalidate uncertain host output without advancing committed scene state.
    /// Supply the candidate that may have partially reached the terminal.
    pub fn invalidate_after_failed_output(&mut self, candidate: &Self) {
        self.repair_ids.extend(self.transmitted.values().copied());
        self.repair_ids
            .extend(candidate.transmitted.values().copied());
        self.repair_ids
            .extend(candidate.allocated_ids.iter().copied());
        self.repair_ids.extend(candidate.repair_ids.iter().copied());
        #[cfg(feature = "kitty")]
        {
            self.next_host_id = self.next_host_id.max(candidate.next_host_id);
        }
    }

    /// Remove stale placements and release inactive content beyond the host budget.
    /// Commit this candidate only after the enclosing output flush succeeds.
    pub fn finish_frame(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        self.emit_repair(out)?;
        #[cfg(feature = "kitty")]
        {
            let stale: Vec<_> = self
                .placements
                .iter()
                .filter(|(key, _)| !self.seen_placements.contains(key))
                .map(|(key, id)| (*key, *id))
                .collect();
            for (key, id) in stale {
                out.write_all(b"\x1b_")?;
                out.write_all(&crate::codec::kitty::encode_delete_placement(key.0, id))?;
                out.write_all(b"\x1b\\")?;
                self.placements.remove(&key);
            }
            let mut inactive: Vec<_> = self
                .transmitted
                .iter()
                .filter(|(id, _)| {
                    !self.frame_seen.contains(id) && !self.frame_protected.contains(id)
                })
                .map(|(id, host)| (*id, *host))
                .collect();
            inactive.sort_unstable();
            for (id, host) in inactive {
                if self.transmitted.len() <= 128
                    && self.content_bytes.values().sum::<usize>() <= 64 * 1024 * 1024
                {
                    break;
                }
                write!(out, "\x1b_Ga=d,d=I,i={host},q=2\x1b\\")?;
                self.transmitted.remove(&id);
                self.content_bytes.remove(&id);
            }
        }
        self.cache.trim();
        Ok(())
    }

    fn emit_repair(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        #[cfg(feature = "kitty")]
        if !self.repair_ids.is_empty() {
            for id in &self.repair_ids {
                write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
            }
            self.transmitted.clear();
            self.content_bytes.clear();
            self.placements.clear();
        }
        #[cfg(not(feature = "kitty"))]
        let _ = out;
        self.repair_ids.clear();
        Ok(())
    }

    /// Release every owned host resource, including retained invisible content.
    /// Perform on a candidate and commit only after flush succeeds.
    pub fn reset(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        self.repair_ids.extend(self.transmitted.values().copied());
        self.emit_repair(out)?;
        self.cache.clear();
        Ok(())
    }

    /// Release retained resources when host protocol selection changes.
    /// Call once per candidate frame even when there are no image placements.
    /// Commit the candidate only after output and flush succeed.
    pub fn select_protocol(
        &mut self,
        out: &mut impl Write,
        protocol: Option<crate::model::ImageProtocol>,
    ) -> std::io::Result<()> {
        if self.protocol != protocol {
            self.reset(out)?;
            self.protocol = protocol;
        }
        self.emit_repair(out)
    }

    /// Deterministic cache work counters: source decodes and fragment preparations.
    #[must_use]
    pub fn work_counts(&self) -> (usize, usize) {
        (self.cache.decodes, self.cache.preparations)
    }

    #[cfg(feature = "kitty")]
    fn place(
        &mut self,
        out: &mut impl Write,
        host: u32,
        x: u16,
        y: u16,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<()> {
        self.place_cropped(out, host, (x, y), (cols, rows), None)
    }

    #[cfg(feature = "kitty")]
    fn place_cropped(
        &mut self,
        out: &mut impl Write,
        host: u32,
        position: (u16, u16),
        size: (u16, u16),
        crop: Option<crate::model::KittySourceRect>,
    ) -> std::io::Result<()> {
        let (x, y) = position;
        let (cols, rows) = size;
        let key = (host, x, y);
        if self.seen_placements.len() >= 1024 && !self.seen_placements.contains(&key) {
            return Err(std::io::Error::other(
                "kitty frame placement budget exceeded",
            ));
        }
        let id = if let Some(id) = self.placements.get(&key) {
            *id
        } else {
            let index = self
                .placements
                .iter()
                .find(|(existing, _)| {
                    existing.0 == host && !self.seen_placements.contains(existing)
                })
                .map(|(key, id)| (*key, *id));
            if let Some((old_key, old_id)) = index {
                out.write_all(b"\x1b_")?;
                out.write_all(&crate::codec::kitty::encode_delete_placement(host, old_id))?;
                out.write_all(b"\x1b\\")?;
                self.placements.remove(&old_key);
            }
            self.next_placement_id = self
                .next_placement_id
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("kitty placement ID space exhausted"))?;
            self.placements.insert(key, self.next_placement_id);
            self.next_placement_id
        };
        self.seen_placements.insert(key);
        // Kitty places relative to the current terminal cursor; the cache key
        // alone does not position output. Apply this for cached and cropped
        // placements too, after any upload/deletion commands.
        write!(out, "\x1b[{};{}H", u32::from(y) + 1, u32::from(x) + 1)?;
        out.write_all(b"\x1b_")?;
        let command = if let Some(crop) = crop {
            crate::codec::kitty::encode_place_with_crop(host, id, cols, rows, crop)
        } else {
            crate::codec::kitty::encode_place_with_z_and_cells(host, id, 0, cols, rows)
        };
        out.write_all(&command)?;
        out.write_all(b"\x1b\\")
    }

    #[cfg(feature = "kitty")]
    fn reserve_content(
        &mut self,
        out: &mut impl Write,
        id: u64,
        cost: usize,
    ) -> std::io::Result<()> {
        const LIMIT: usize = 64 * 1024 * 1024;
        if cost > LIMIT {
            return Err(std::io::Error::other("kitty content exceeds byte budget"));
        }
        let mut inactive: Vec<_> = self
            .transmitted
            .keys()
            .filter(|key| {
                **key != id && !self.frame_seen.contains(key) && !self.frame_protected.contains(key)
            })
            .copied()
            .collect();
        inactive.sort_unstable();
        let old_cost = self.content_bytes.get(&id).copied().unwrap_or(0);
        for stale in inactive {
            let bytes = self
                .content_bytes
                .values()
                .sum::<usize>()
                .saturating_sub(old_cost);
            if bytes <= LIMIT - cost
                && (self.transmitted.contains_key(&id) || self.transmitted.len() < 128)
            {
                break;
            }
            let host = self.transmitted[&stale];
            write!(out, "\x1b_Ga=d,d=I,i={host},q=2\x1b\\")?;
            self.transmitted.remove(&stale);
            self.content_bytes.remove(&stale);
            self.placements.retain(|key, _| key.0 != host);
            self.seen_placements.retain(|key| key.0 != host);
        }
        let retained = self
            .content_bytes
            .values()
            .sum::<usize>()
            .saturating_sub(old_cost);
        if cost > LIMIT || retained > LIMIT - cost {
            return Err(std::io::Error::other(
                "kitty retained content byte budget exceeded",
            ));
        }
        if !self.transmitted.contains_key(&id) && self.transmitted.len() >= 128 {
            return Err(std::io::Error::other(
                "kitty retained content entry budget exceeded",
            ));
        }
        self.content_bytes.insert(id, cost);
        Ok(())
    }

    /// Get or allocate a host-side kitty image ID for a bmux image.
    #[cfg(feature = "kitty")]
    fn get_or_allocate(&mut self, bmux_image_id: u64) -> std::io::Result<(u32, bool)> {
        if self.frame_seen.len() >= 128 && !self.frame_seen.contains(&bmux_image_id) {
            return Err(std::io::Error::other("kitty frame content budget exceeded"));
        }
        self.frame_seen.insert(bmux_image_id);
        if let Some(&host_id) = self.transmitted.get(&bmux_image_id) {
            Ok((host_id, false)) // Already transmitted.
        } else {
            self.next_host_id = self
                .next_host_id
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("kitty image ID space exhausted"))?;
            let host_id = self.next_host_id;
            self.allocated_ids.insert(host_id);
            self.transmitted.insert(bmux_image_id, host_id);
            Ok((host_id, true)) // Newly allocated, needs transmission.
        }
    }
}

/// Render images for a single pane as an overlay on the host terminal.
///
/// Images are emitted after the cell content has been drawn.  The cursor
/// is assumed to be hidden (Fix 1) and the frame is inside a synchronized
/// update (Fix 2).
pub fn render_pane_images(
    out: &mut impl Write,
    images: &[PaneImage],
    pane_rect: PaneRect,
    host_caps: &HostImageCapabilities,
    decode_mode: ImageDecodeMode,
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    kitty_state.select_protocol(out, host_caps.preferred_protocol())?;
    if images.is_empty() {
        return Ok(());
    }

    // `pane_rect` is the pane's **content rect** — the PTY interior, not
    // the outer surface bounds. Callers are responsible for passing the
    // `content_rect` from the scene so decoration thickness is handled by
    // the scene producer once, not recomputed here.
    let inner_x = pane_rect.x;
    let inner_y = pane_rect.y;
    let inner_w = pane_rect.w;
    let inner_h = pane_rect.h;

    for image in images {
        // Skip images entirely outside the pane inner area.
        if image.position.row >= inner_h || image.position.col >= inner_w {
            continue;
        }

        // Allow partially-overlapping images: the pane border will
        // visually clip the overflow.  Only skip fully-outside images.
        let host_x = inner_x.saturating_add(image.position.col);
        let host_y = inner_y.saturating_add(image.position.row);

        match decode_mode {
            ImageDecodeMode::Passthrough => {
                emit_passthrough(out, image, host_x, host_y, host_caps, kitty_state)?;
            }
            ImageDecodeMode::Server => {
                emit_from_pixels(out, image, host_x, host_y, host_caps, kitty_state)?;
            }
            ImageDecodeMode::Client => {
                emit_client_decode(out, image, host_x, host_y, host_caps, kitty_state)?;
            }
        }
    }

    Ok(())
}

/// Decode and render visible fragments using the host's selected protocol.
pub fn render_pane_images_clipped(
    out: &mut impl Write,
    images: &[PaneImage],
    pane: PaneRect,
    covers: &[bmux_tui::geometry::Rect],
    caps: &HostImageCapabilities,
    state: &mut KittyHostState,
) -> std::io::Result<()> {
    use bmux_tui::geometry::Rect;
    state.select_protocol(out, caps.preferred_protocol())?;
    for image in images {
        let destination = Rect::new(
            pane.x.saturating_add(image.position.col),
            pane.y.saturating_add(image.position.row),
            image.cell_size.cols,
            image.cell_size.rows,
        );
        render_image_clipped(
            out,
            image,
            destination,
            Rect::new(pane.x, pane.y, pane.w, pane.h),
            covers,
            caps,
            state,
            None,
        )?;
    }
    Ok(())
}

/// Shared source entry point used by the TUI adapter; geometry is not content.
#[allow(clippy::too_many_arguments)] // Placement and borrowed source remain separate from wire models.
pub(crate) fn render_image_clipped(
    out: &mut impl Write,
    image: &PaneImage,
    destination: bmux_tui::geometry::Rect,
    clip: bmux_tui::geometry::Rect,
    covers: &[bmux_tui::geometry::Rect],
    caps: &HostImageCapabilities,
    state: &mut KittyHostState,
    shared: Option<&std::sync::Arc<PaneImage>>,
) -> std::io::Result<()> {
    use bmux_tui::geometry::Rect;
    state.select_protocol(out, caps.preferred_protocol())?;
    let mut fragments = vec![destination.intersection(clip)];
    for cover in covers {
        fragments = fragments
            .into_iter()
            .flat_map(|rect| clipping::subtract(rect, *cover))
            .collect();
        if fragments.len() > 256 {
            return Err(std::io::Error::other(
                "image occlusion fragment budget exceeded",
            ));
        }
    }
    for visible in fragments.into_iter().filter(|rect| !rect.is_empty()) {
        let crop = Rect::new(
            visible.x - destination.x,
            visible.y - destination.y,
            visible.width,
            visible.height,
        );
        let size = (destination.width, destination.height);
        let host = (
            caps.preferred_protocol(),
            caps.cell_pixel_width,
            caps.cell_pixel_height,
        );
        let prepared = if let Some(fragment) = state.cache.find(image, shared, crop, size, host) {
            fragment
        } else {
            let decoded = state.cache.decoded(image, shared)?;
            let (prepared_pixels, pixel_size) = if visible != destination {
                let mut payload = crate::model::ImagePayload {
                    raw: None,
                    pixels: Some((*decoded).clone()),
                };
                let pixel_size = crate::tui::crop_to_visible(&mut payload, destination, visible);
                (
                    std::sync::Arc::new(payload.pixels.take().expect("cropped pixels")),
                    pixel_size,
                )
            } else {
                (decoded, image.pixel_size)
            };
            let fragment = PaneImage {
                id: image.id,
                protocol: image.protocol,
                payload: crate::model::ImagePayload::default(),
                position: image.position,
                cell_size: crate::model::ImageCellSize {
                    cols: visible.width,
                    rows: visible.height,
                },
                pixel_size,
            };
            let mut encoded = Vec::new();
            if caps.preferred_protocol() != Some(crate::model::ImageProtocol::KittyGraphics) {
                emit_pixels(
                    &mut encoded,
                    &fragment,
                    &prepared_pixels,
                    0,
                    0,
                    caps,
                    &mut KittyHostState::default(),
                )?;
                // Position is placement state, never part of cached content.
                if encoded.starts_with(b"\x1b[1;1H") {
                    encoded.drain(..6);
                }
            }
            state
                .cache
                .insert(image, shared, crop, size, host, prepared_pixels, encoded)
        };
        if caps.preferred_protocol() == Some(crate::model::ImageProtocol::KittyGraphics) {
            let fragment = PaneImage {
                id: prepared.id,
                payload: crate::model::ImagePayload::default(),
                cell_size: crate::model::ImageCellSize {
                    cols: visible.width,
                    rows: visible.height,
                },
                protocol: image.protocol,
                position: image.position,
                pixel_size: image.pixel_size,
            };
            #[cfg(feature = "kitty")]
            if visible == destination {
                let png = image
                    .payload
                    .pixels
                    .as_ref()
                    .filter(|pixels| pixels.format == crate::model::PixelFormat::Png)
                    .map(|pixels| pixels.data.as_slice())
                    .or_else(|| {
                        (image.protocol == crate::model::ImageProtocol::KittyGraphics)
                            .then_some(image.payload.raw.as_deref())
                            .flatten()
                    });
                if let Some(png) = png {
                    write!(out, "\x1b[{};{}H", visible.y + 1, visible.x + 1)?;
                    emit_kitty_data(
                        out,
                        &fragment,
                        crate::model::KittyFormat::Png,
                        png,
                        prepared.pixels.width,
                        prepared.pixels.height,
                        visible.x,
                        visible.y,
                        state,
                    )?;
                    continue;
                }
            }
            emit_pixels(
                out,
                &fragment,
                &prepared.pixels,
                visible.x,
                visible.y,
                caps,
                state,
            )?;
        } else {
            write!(out, "\x1b[{};{}H", visible.y + 1, visible.x + 1)?;
            out.write_all(&prepared.encoded)?;
        }
    }
    Ok(())
}

/// Passthrough mode: re-emit raw protocol bytes at translated coordinates.
#[allow(unused_variables)]
fn emit_passthrough(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    _host_caps: &HostImageCapabilities,
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    let Some(raw) = &image.payload.raw else {
        return Ok(());
    };

    // Move cursor to the image position.
    write!(out, "\x1b[{};{}H", host_y + 1, host_x + 1)?;

    match image.protocol {
        #[cfg(feature = "sixel")]
        crate::model::ImageProtocol::Sixel => {
            // Re-emit the sixel DCS sequence.
            out.write_all(b"\x1bPq")?;
            out.write_all(raw)?;
            out.write_all(b"\x1b\\")?;
        }
        #[cfg(feature = "kitty")]
        crate::model::ImageProtocol::KittyGraphics => {
            emit_kitty_data(
                out,
                image,
                crate::model::KittyFormat::Png,
                raw,
                image.pixel_size.width,
                image.pixel_size.height,
                host_x,
                host_y,
                kitty_state,
            )?;
        }
        #[cfg(feature = "iterm2")]
        crate::model::ImageProtocol::ITerm2 => {
            // Re-emit iTerm2 OSC 1337.
            out.write_all(b"\x1b]1337;File=")?;
            out.write_all(raw)?;
            out.write_all(b"\x07")?;
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }

    Ok(())
}

/// Server-decode mode: encode decoded pixels for the host's preferred protocol.
#[allow(unused_variables, unreachable_code)]
fn emit_from_pixels(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    host_caps: &HostImageCapabilities,
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    let Some(pixels) = &image.payload.pixels else {
        return emit_passthrough(out, image, host_x, host_y, host_caps, kitty_state);
    };

    emit_pixels(out, image, pixels, host_x, host_y, host_caps, kitty_state)
}

#[allow(unused_variables)]
fn emit_pixels(
    out: &mut impl Write,
    image: &PaneImage,
    pixels: &crate::model::PixelBuffer,
    host_x: u16,
    host_y: u16,
    host_caps: &HostImageCapabilities,
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    write!(out, "\x1b[{};{}H", host_y + 1, host_x + 1)?;

    match host_caps.preferred_protocol() {
        #[cfg(feature = "sixel")]
        Some(crate::model::ImageProtocol::Sixel) => {
            let target_width =
                u32::from(image.cell_size.cols) * u32::from(host_caps.cell_pixel_width);
            let target_height =
                u32::from(image.cell_size.rows) * u32::from(host_caps.cell_pixel_height);
            let scaled;
            let pixels = if image.protocol != crate::model::ImageProtocol::Sixel
                && target_width > 0
                && target_height > 0
                && (target_width != pixels.width || target_height != pixels.height)
            {
                if u64::from(target_width) * u64::from(target_height) > 16 * 1024 * 1024 {
                    return Err(std::io::Error::other(
                        "sixel placement exceeds pixel budget",
                    ));
                }
                let decoded = if pixels.format == crate::model::PixelFormat::Png {
                    clipping::pixels(&crate::model::ImagePayload {
                        raw: None,
                        pixels: Some(pixels.clone()),
                    })?
                } else {
                    pixels.clone()
                };
                let rgba = match decoded.format {
                    crate::model::PixelFormat::Rgb8 => image::DynamicImage::ImageRgb8(
                        image::RgbImage::from_raw(decoded.width, decoded.height, decoded.data)
                            .ok_or_else(|| std::io::Error::other("invalid RGB image"))?,
                    )
                    .to_rgba8(),
                    _ => image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.data)
                        .ok_or_else(|| std::io::Error::other("invalid RGBA image"))?,
                };
                scaled = crate::model::PixelBuffer {
                    data: image::imageops::resize(
                        &rgba,
                        target_width,
                        target_height,
                        image::imageops::FilterType::Nearest,
                    )
                    .into_raw(),
                    width: target_width,
                    height: target_height,
                    format: crate::model::PixelFormat::Rgba8,
                };
                &scaled
            } else {
                pixels
            };
            if let Some(sixel_data) = crate::codec::sixel::encode(pixels) {
                out.write_all(b"\x1bPq")?;
                out.write_all(&sixel_data)?;
                out.write_all(b"\x1b\\")?;
            }
        }
        #[cfg(feature = "kitty")]
        Some(crate::model::ImageProtocol::KittyGraphics) => {
            let (kitty_format, data) = match pixels.format {
                crate::model::PixelFormat::Rgb8 => {
                    (crate::model::KittyFormat::Rgb, pixels.data.as_slice())
                }
                crate::model::PixelFormat::Rgba8 => {
                    (crate::model::KittyFormat::Rgba, pixels.data.as_slice())
                }
                crate::model::PixelFormat::Png => {
                    (crate::model::KittyFormat::Png, pixels.data.as_slice())
                }
            };
            emit_kitty_data(
                out,
                image,
                kitty_format,
                data,
                pixels.width,
                pixels.height,
                host_x,
                host_y,
                kitty_state,
            )?;
        }
        #[cfg(feature = "iterm2")]
        Some(crate::model::ImageProtocol::ITerm2) => {
            let png = pixels_as_png(pixels)?;
            out.write_all(b"\x1b]1337;File=")?;
            out.write_all(&crate::codec::iterm2::encode_body_with_cells(
                &png,
                image.cell_size.cols,
                image.cell_size.rows,
            ))?;
            out.write_all(b"\x07")?;
        }
        _ => {}
    }

    Ok(())
}

#[cfg(feature = "kitty")]
#[allow(clippy::too_many_arguments)] // Protocol metadata and caller-owned placement are separate inputs.
fn emit_kitty_data(
    out: &mut impl Write,
    image: &PaneImage,
    format: crate::model::KittyFormat,
    data: &[u8],
    width: u32,
    height: u32,
    x: u16,
    y: u16,
    state: &mut KittyHostState,
) -> std::io::Result<()> {
    let host_id = upload_kitty_data(out, image, format, data, width, height, state)?;
    state.place(
        out,
        host_id,
        x,
        y,
        image.cell_size.cols,
        image.cell_size.rows,
    )
}

#[cfg(feature = "kitty")]
fn upload_kitty_data(
    out: &mut impl Write,
    image: &PaneImage,
    format: crate::model::KittyFormat,
    data: &[u8],
    width: u32,
    height: u32,
    state: &mut KittyHostState,
) -> std::io::Result<u32> {
    let cost = data.len().max(
        usize::try_from(
            u64::from(width)
                .saturating_mul(u64::from(height))
                .saturating_mul(4),
        )
        .unwrap_or(usize::MAX),
    );
    let frame_bytes = state
        .frame_seen
        .iter()
        .filter(|id| **id != image.id)
        .map(|id| state.content_bytes.get(id).copied().unwrap_or(0))
        .sum::<usize>();
    if cost > 64 * 1024 * 1024 || frame_bytes > 64 * 1024 * 1024 - cost {
        return Err(std::io::Error::other(
            "kitty visible content byte budget exceeded",
        ));
    }
    state.reserve_content(out, image.id, cost)?;
    let (host_id, transmit) = state.get_or_allocate(image.id)?;
    if transmit {
        for chunk in
            crate::codec::kitty::encode_transmit_chunks(host_id, format, data, width, height)
        {
            out.write_all(b"\x1b_")?;
            out.write_all(&chunk)?;
            out.write_all(b"\x1b\\")?;
        }
    }
    Ok(host_id)
}

#[cfg(feature = "iterm2")]
fn pixels_as_png(pixels: &crate::model::PixelBuffer) -> std::io::Result<Vec<u8>> {
    if pixels.format == crate::model::PixelFormat::Png {
        return Ok(pixels.data.clone());
    }
    let color = match pixels.format {
        crate::model::PixelFormat::Rgb8 => image::ColorType::Rgb8,
        crate::model::PixelFormat::Rgba8 => image::ColorType::Rgba8,
        crate::model::PixelFormat::Png => unreachable!(),
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels.data, pixels.width, pixels.height, color.into())
        .map_err(std::io::Error::other)?;
    Ok(png)
}

/// Client-decode mode: decode raw bytes, then encode for host protocol.
fn emit_client_decode(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    host_caps: &HostImageCapabilities,
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    // TODO: decode raw bytes to pixels, then delegate to emit_from_pixels.
    // For now, fall back to passthrough.
    emit_passthrough(out, image, host_x, host_y, host_caps, kitty_state)
}
