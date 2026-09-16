//! Per-pane image registry.
//!
//! Stores active images, tracks scroll-based position shifts, evicts
//! images that scroll past the scrollback limit, and provides delta
//! queries for efficient IPC transport.

use crate::model::{
    ImageCellSize, ImageDelta, ImageEvent, ImagePayload, ImagePixelSize, ImagePosition,
    ImageProtocol, PaneImage,
};

/// A change log entry for delta tracking.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Used when multiple image features are enabled; dead in single-feature combos
enum ChangeLogEntry {
    Added { sequence: u64, image: PaneImage },
    Removed { sequence: u64, image_id: u64 },
}

/// Accumulator for kitty chunked transmissions.
#[cfg(feature = "kitty")]
#[derive(Clone)]
struct KittyChunkAccumulator {
    data: Vec<u8>,
    format: crate::model::KittyFormat,
    width: u32,
    height: u32,
}

/// Per-pane image storage with scroll tracking and delta queries.
#[derive(Clone)]
#[allow(dead_code)] // Fields used when image features are enabled; dead in minimal feature combos
pub struct ImageRegistry {
    images: Vec<PaneImage>,
    /// Original placements retained independently of the live projection.
    history: std::collections::BTreeMap<u64, (PaneImage, i64)>,
    /// Hidden normal-screen state while an alternate screen is active.
    normal_screen: Option<Box<ImageRegistry>>,
    /// Height used for subsequent live scroll projections.
    viewport_height: u16,
    next_id: u64,
    /// Monotonic sequence counter; incremented on every mutation.
    sequence: u64,
    /// Maximum number of images kept per pane.
    max_images: usize,
    /// Maximum bytes of image payload per pane (0 = unlimited).
    max_bytes: usize,
    /// Change log for delta tracking: (sequence, event).
    change_log: Vec<ChangeLogEntry>,
    /// Maximum change log entries before compaction.
    max_change_log: usize,

    // Kitty-specific state
    #[cfg(feature = "kitty")]
    kitty_transmitted: std::collections::BTreeMap<u32, crate::model::KittyTransmittedImage>,
    #[cfg(feature = "kitty")]
    kitty_placements: std::collections::BTreeMap<(u32, u32), u64>,
    /// Accumulator for kitty chunked transmissions.
    #[cfg(feature = "kitty")]
    kitty_pending_chunks: std::collections::BTreeMap<u32, KittyChunkAccumulator>,
}

/// Immutable retained placements for a text capture. No live delta log or
/// in-progress protocol transfers are copied into a history pin.
#[derive(Clone)]
pub struct ImageHistorySnapshot {
    registry: ImageRegistry,
}

impl ImageHistorySnapshot {
    /// Project captured placements through a caller-owned content mapping.
    /// Mapping returns a viewport-relative row and column; source geometry stays
    /// unchanged. The caller supplies the same captured text mapping for every
    /// placement and charges its work independently of this allocation budget.
    pub fn project_mapped(
        &self,
        width: u16,
        height: u16,
        budget: &mut usize,
        mut map: impl FnMut(i64, u16) -> std::io::Result<(i64, u16)>,
    ) -> std::io::Result<Vec<PaneImage>> {
        if width == 0 || height == 0 {
            return Ok(Vec::new());
        }
        let mut projected = self.registry.capture_history(budget)?;
        for (image, row) in projected.registry.history.values_mut() {
            let (mapped_row, column) = map(*row, image.position.col)?;
            *row = mapped_row;
            image.position.col = column;
        }
        // Exclude horizontally invisible placements before vertical cropping,
        // which may decode payloads and must not fail for an invisible image.
        projected
            .registry
            .history
            .retain(|_, (image, _)| image.position.col < width);
        let mut images = projected.registry.project_viewport(0, height)?;
        for image in &mut images {
            let columns = image.cell_size.cols.min(width - image.position.col);
            if columns < image.cell_size.cols {
                image.payload.pixels = Some(crate::compositor::clipping::decoded(image)?);
                image.payload.raw = None;
                image.pixel_size = crate::tui::crop_to_visible(
                    &mut image.payload,
                    bmux_tui::geometry::Rect::new(0, 0, image.cell_size.cols, image.cell_size.rows),
                    bmux_tui::geometry::Rect::new(0, 0, columns, image.cell_size.rows),
                );
                image.cell_size.cols = columns;
                image.payload.raw = Some(encode_projected_payload(image)?);
            }
        }
        Ok(images)
    }

    /// Project this capture without observing subsequent output or deletion.
    pub fn project_viewport(&self, offset: usize, height: u16) -> std::io::Result<Vec<PaneImage>> {
        self.registry.project_viewport(offset, height)
    }
}

impl ImageRegistry {
    /// Whether normal-screen placements need remapping, including hidden history.
    #[must_use]
    pub fn has_main_images(&self) -> bool {
        !self
            .normal_screen
            .as_deref()
            .unwrap_or(self)
            .history
            .is_empty()
    }

    /// Copy retained placements only after charging their complete allocation.
    /// The caller must capture text under the same execution-state lock boundary.
    pub fn capture_history(&self, budget: &mut usize) -> std::io::Result<ImageHistorySnapshot> {
        let source = self.normal_screen.as_deref().unwrap_or(self);
        let bytes = source
            .history
            .values()
            .try_fold(std::mem::size_of::<ImageRegistry>(), |total, (image, _)| {
                total
                    .checked_add(std::mem::size_of::<PaneImage>() + 128)
                    .and_then(|total| {
                        total.checked_add(image.payload.raw.as_ref().map_or(0, Vec::len))
                    })
                    .and_then(|total| {
                        total.checked_add(
                            image
                                .payload
                                .pixels
                                .as_ref()
                                .map_or(0, |pixels| pixels.data.len()),
                        )
                    })
            })
            .ok_or_else(|| std::io::Error::other("image capture size overflow"))?;
        let remaining = budget
            .checked_sub(bytes)
            .ok_or_else(|| std::io::Error::other("image capture budget exhausted"))?;
        let mut registry = Self::new(source.max_images, source.max_bytes);
        registry.history = source.history.clone();
        registry.sequence = source.sequence;
        registry.next_id = source.next_id;
        *budget = remaining;
        Ok(ImageHistorySnapshot { registry })
    }

    /// Create a new empty registry.
    pub fn new(max_images: usize, max_bytes: usize) -> Self {
        Self {
            images: Vec::new(),
            history: std::collections::BTreeMap::new(),
            normal_screen: None,
            viewport_height: u16::MAX,
            next_id: 1,
            sequence: 0,
            max_images,
            max_bytes,
            change_log: Vec::new(),
            max_change_log: 1000,
            #[cfg(feature = "kitty")]
            kitty_transmitted: std::collections::BTreeMap::new(),
            #[cfg(feature = "kitty")]
            kitty_placements: std::collections::BTreeMap::new(),
            #[cfg(feature = "kitty")]
            kitty_pending_chunks: std::collections::BTreeMap::new(),
        }
    }

    /// Set the initial viewport while constructing an empty registry.
    #[must_use]
    pub fn with_viewport_height(mut self, height: u16) -> Self {
        self.viewport_height = height;
        self
    }

    /// Switch screen-local image state, preserving the hidden normal screen.
    /// Placement IDs and change sequence remain monotonic across switches.
    pub fn set_alternate_screen(&mut self, alternate: bool) {
        if alternate == self.normal_screen.is_some() {
            return;
        }
        let old_ids = self.images.iter().map(|image| image.id).collect::<Vec<_>>();
        let sequence = self.sequence;
        let next_id = self.next_id;
        let log = std::mem::take(&mut self.change_log);
        if alternate {
            let mut empty = Self::new(self.max_images, self.max_bytes);
            empty.viewport_height = self.viewport_height;
            let normal = std::mem::replace(self, empty);
            self.normal_screen = Some(Box::new(normal));
        } else if let Some(normal) = self.normal_screen.take() {
            *self = *normal;
        }
        self.next_id = next_id;
        self.sequence = sequence + 1;
        self.change_log = log;
        for image_id in old_ids {
            self.change_log.push(ChangeLogEntry::Removed {
                sequence: self.sequence,
                image_id,
            });
        }
        for image in &self.images {
            self.change_log.push(ChangeLogEntry::Added {
                sequence: self.sequence,
                image: image.clone(),
            });
        }
        self.compact_change_log();
    }

    /// Handle an image event produced by the interceptor.
    ///
    /// `cell_width` and `cell_height` are the pane's cell dimensions in
    /// pixels, used to compute `cell_size` from pixel dimensions.
    #[allow(unused_variables)]
    pub fn handle_event(
        &mut self,
        event: ImageEvent,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) {
        match event {
            #[cfg(feature = "sixel")]
            ImageEvent::SixelImage {
                data,
                position,
                pixel_size,
                ..
            } => {
                let cell_size =
                    pixel_size_to_cells(pixel_size, cell_pixel_width, cell_pixel_height);
                self.add_image(
                    ImageProtocol::Sixel,
                    ImagePayload {
                        raw: Some(data),
                        pixels: None,
                    },
                    position,
                    cell_size,
                    pixel_size,
                );
            }

            #[cfg(feature = "kitty")]
            ImageEvent::KittyCommand { command: cmd, .. } => {
                self.handle_kitty_command(cmd, cell_pixel_width, cell_pixel_height);
            }

            #[cfg(feature = "iterm2")]
            ImageEvent::ITerm2Image { data, position, .. } => {
                let Some((params, bytes)) = crate::codec::iterm2::parse_body(&data) else {
                    return;
                };
                if !params.inline {
                    return;
                }
                let Some(pixel_size) = crate::codec::iterm2::estimate_pixel_size(&bytes) else {
                    return;
                };
                let mut cell_size =
                    pixel_size_to_cells(pixel_size, cell_pixel_width, cell_pixel_height);
                // Bare numeric dimensions in OSC 1337 are terminal cells.
                if let Some(cols) = params
                    .width
                    .as_deref()
                    .and_then(|value| value.parse::<u16>().ok())
                    .filter(|value| *value > 0)
                {
                    cell_size.cols = cols;
                }
                if let Some(rows) = params
                    .height
                    .as_deref()
                    .and_then(|value| value.parse::<u16>().ok())
                    .filter(|value| *value > 0)
                {
                    cell_size.rows = rows;
                }
                self.add_image(
                    ImageProtocol::ITerm2,
                    ImagePayload {
                        raw: Some(data),
                        pixels: None,
                    },
                    position,
                    cell_size,
                    pixel_size,
                );
            }
        }
    }

    /// Insert a new image into the registry.
    #[allow(dead_code)] // Called from feature-gated image processing paths
    fn add_image(
        &mut self,
        protocol: ImageProtocol,
        payload: ImagePayload,
        position: ImagePosition,
        cell_size: ImageCellSize,
        pixel_size: ImagePixelSize,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.sequence += 1;

        let image = PaneImage {
            id,
            protocol,
            payload,
            position,
            cell_size,
            pixel_size,
        };

        self.history
            .insert(id, (image.clone(), i64::from(position.row)));
        if position.row < self.viewport_height {
            self.change_log.push(ChangeLogEntry::Added {
                sequence: self.sequence,
                image: image.clone(),
            });
            self.images.push(image);
        }
        self.compact_change_log();
        self.enforce_limits();
    }

    /// Remove images exceeding the per-pane limits (oldest first).
    #[allow(dead_code)] // Called from add_image which is feature-gated
    fn enforce_limits(&mut self) {
        while self.images.len() > self.max_images {
            let removed = self.images.remove(0);
            self.sequence += 1;
            self.change_log.push(ChangeLogEntry::Removed {
                sequence: self.sequence,
                image_id: removed.id,
            });
        }

        if self.max_bytes > 0 {
            while self.total_bytes() > self.max_bytes && !self.images.is_empty() {
                let removed = self.images.remove(0);
                self.sequence += 1;
                self.change_log.push(ChangeLogEntry::Removed {
                    sequence: self.sequence,
                    image_id: removed.id,
                });
            }
        }
        while self.history.len() > self.max_images
            || (self.max_bytes > 0
                && self
                    .history
                    .values()
                    .map(|(image, _)| {
                        image.payload.raw.as_ref().map_or(0, Vec::len)
                            + image
                                .payload
                                .pixels
                                .as_ref()
                                .map_or(0, |pixels| pixels.data.len())
                    })
                    .sum::<usize>()
                    > self.max_bytes)
        {
            let Some((id, _)) = self.history.pop_first() else {
                break;
            };
            self.images.retain(|image| image.id != id);
            self.sequence += 1;
            self.change_log.push(ChangeLogEntry::Removed {
                sequence: self.sequence,
                image_id: id,
            });
        }
        #[cfg(feature = "kitty")]
        self.kitty_placements
            .retain(|_, id| self.history.contains_key(id));
        self.compact_change_log();
    }

    /// Compact the change log if it exceeds the maximum size.
    fn compact_change_log(&mut self) {
        if self.change_log.len() > self.max_change_log {
            // Drop the oldest half.
            let keep = self.max_change_log / 2;
            self.change_log.drain(..self.change_log.len() - keep);
        }
    }

    #[allow(dead_code)] // Called from enforce_limits which is feature-gated
    fn total_bytes(&self) -> usize {
        self.images
            .iter()
            .map(|img| {
                img.payload.raw.as_ref().map_or(0, |r| r.len())
                    + img.payload.pixels.as_ref().map_or(0, |p| p.data.len())
            })
            .sum()
    }

    /// Advance retained anchors and rebuild the live crop from original pixels.
    /// Fully offscreen originals remain available for history projection.
    pub fn scroll_up(&mut self, lines: u16) -> std::io::Result<()> {
        if lines == 0 {
            return Ok(());
        }
        let original_rows = self
            .history
            .iter()
            .map(|(&id, (_, row))| (id, *row))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (_, row) in self.history.values_mut() {
            *row = row.saturating_sub(i64::from(lines));
        }
        let projected = match self.project_viewport(0, self.viewport_height) {
            Ok(projected) => projected,
            Err(error) => {
                for (id, (_, row)) in &mut self.history {
                    *row = original_rows[id];
                }
                return Err(error);
            }
        };
        self.sequence += 1;
        for old in &self.images {
            if !projected.iter().any(|image| image.id == old.id) {
                self.change_log.push(ChangeLogEntry::Removed {
                    sequence: self.sequence,
                    image_id: old.id,
                });
            }
        }
        for image in &projected {
            self.change_log.push(ChangeLogEntry::Added {
                sequence: self.sequence,
                image: image.clone(),
            });
        }
        self.images = projected;
        self.compact_change_log();
        Ok(())
    }

    /// Reproject the active alternate screen after a height change without
    /// moving its placements or touching hidden normal-screen history.
    pub fn resize_alternate_viewport(&mut self, height: u16) -> std::io::Result<()> {
        if self.normal_screen.is_none() {
            if self.history.is_empty() {
                self.viewport_height = height;
            }
            return Ok(());
        }
        let projected = self.project_viewport(0, height)?;
        // An empty hidden screen still needs the new geometry. Otherwise
        // leaving alternate mode restores a stale height for future images.
        if let Some(normal) = self.normal_screen.as_mut()
            && normal.history.is_empty()
        {
            normal.viewport_height = height;
        }
        self.commit_alternate_projection(projected);
        self.viewport_height = height;
        Ok(())
    }

    fn commit_alternate_projection(&mut self, projected: Vec<PaneImage>) {
        if projected == self.images {
            return;
        }
        self.sequence += 1;
        for old in &self.images {
            if !projected.iter().any(|image| image.id == old.id) {
                self.change_log.push(ChangeLogEntry::Removed {
                    sequence: self.sequence,
                    image_id: old.id,
                });
            }
        }
        for image in &projected {
            self.change_log.push(ChangeLogEntry::Added {
                sequence: self.sequence,
                image: image.clone(),
            });
        }
        self.images = projected;
        self.compact_change_log();
    }

    /// Atomically remap retained normal-screen anchors during a geometry change.
    /// The caller maps against the pre-resize text capture. A failed mapping or
    /// crop leaves all placement state and revisions unchanged.
    pub fn remap_positions(
        &mut self,
        height: u16,
        mut map: impl FnMut(i64, u16) -> std::io::Result<(i64, u16)>,
    ) -> std::io::Result<()> {
        if self.normal_screen.is_some() {
            let projected = self.project_viewport(0, height)?;
            if let Some(normal) = self.normal_screen.as_mut() {
                normal.remap_positions(height, map)?;
            }
            self.commit_alternate_projection(projected);
            self.viewport_height = height;
            return Ok(());
        }
        let positions = self
            .history
            .iter()
            .map(|(&id, (image, row))| map(*row, image.position.col).map(|position| (id, position)))
            .collect::<std::io::Result<std::collections::BTreeMap<_, _>>>()?;
        let original = self
            .history
            .iter()
            .map(|(&id, (image, row))| (id, (*row, image.position.col)))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (id, (image, row)) in &mut self.history {
            let (mapped_row, column) = positions[id];
            *row = mapped_row;
            image.position.col = column;
        }
        let projected = match self.project_viewport(0, height) {
            Ok(projected) => projected,
            Err(error) => {
                for (id, (image, row)) in &mut self.history {
                    let (old_row, column) = original[id];
                    *row = old_row;
                    image.position.col = column;
                }
                return Err(error);
            }
        };
        self.sequence += 1;
        for old in &self.images {
            if !projected.iter().any(|image| image.id == old.id) {
                self.change_log.push(ChangeLogEntry::Removed {
                    sequence: self.sequence,
                    image_id: old.id,
                });
            }
        }
        for image in &projected {
            self.change_log.push(ChangeLogEntry::Added {
                sequence: self.sequence,
                image: image.clone(),
            });
        }
        self.images = projected;
        self.viewport_height = height;
        self.compact_change_log();
        Ok(())
    }

    /// Replace the registry contents with a caller-projected image scene.
    ///
    /// Images are matched by ID. New and changed images are recorded as
    /// additions, absent IDs as removals, and byte/count limits are enforced.
    pub fn replace_images(&mut self, images: Vec<PaneImage>) {
        let previous = self
            .images
            .iter()
            .map(|image| (image.id, image.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let next_ids = images
            .iter()
            .map(|image| image.id)
            .collect::<std::collections::BTreeSet<_>>();
        let mut changed = false;

        for image in &self.images {
            if !next_ids.contains(&image.id) {
                self.sequence = self.sequence.saturating_add(1);
                self.change_log.push(ChangeLogEntry::Removed {
                    sequence: self.sequence,
                    image_id: image.id,
                });
                changed = true;
            }
        }
        for image in &images {
            if previous.get(&image.id) != Some(image) {
                self.sequence = self.sequence.saturating_add(1);
                self.change_log.push(ChangeLogEntry::Added {
                    sequence: self.sequence,
                    image: image.clone(),
                });
                changed = true;
            }
        }
        self.history = images
            .iter()
            .map(|image| (image.id, (image.clone(), i64::from(image.position.row))))
            .collect();
        self.images = images;
        if changed {
            self.compact_change_log();
            self.enforce_limits();
        }
    }

    /// Get all images currently in the registry.
    pub fn images(&self) -> &[PaneImage] {
        &self.images
    }

    /// Get images visible within a viewport of `height` rows starting at
    /// scrollback `offset` (0 = bottom/live).
    pub fn images_in_viewport(&self, offset: usize, height: u16) -> Vec<&PaneImage> {
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        self.history
            .values()
            .filter_map(|(image, row)| {
                let top = row.saturating_add(offset);
                (top < i64::from(height) && top.saturating_add(i64::from(image.cell_size.rows)) > 0)
                    .then_some(image)
            })
            .collect()
    }

    /// Build a cropped viewport without modifying original pixels or placement size.
    /// Decode failures are explicit rather than presenting a healthy empty image.
    pub fn project_viewport(&self, offset: usize, height: u16) -> std::io::Result<Vec<PaneImage>> {
        if height == 0 {
            return Ok(Vec::new());
        }
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        self.history
            .values()
            .filter_map(|(image, row)| {
                let top = row.saturating_add(offset);
                let bottom = top.saturating_add(i64::from(image.cell_size.rows));
                if top >= i64::from(height) || bottom <= 0 {
                    return None;
                }
                let skipped = u16::try_from(top.saturating_neg().max(0)).unwrap_or(u16::MAX);
                let visible_rows =
                    u16::try_from(bottom.min(i64::from(height)) - top.max(0)).unwrap_or(0);
                let mut projected = image.clone();
                projected.position.row = u16::try_from(top.max(0)).unwrap_or(u16::MAX);
                if skipped > 0 || visible_rows != image.cell_size.rows {
                    let pixels = match crate::compositor::clipping::decoded(image) {
                        Ok(pixels) => pixels,
                        Err(error) => return Some(Err(error)),
                    };
                    projected.payload.pixels = Some(pixels);
                    projected.payload.raw = None;
                    projected.pixel_size = crate::tui::crop_to_visible(
                        &mut projected.payload,
                        bmux_tui::geometry::Rect::new(
                            0,
                            0,
                            image.cell_size.cols,
                            image.cell_size.rows,
                        ),
                        bmux_tui::geometry::Rect::new(
                            0,
                            skipped,
                            image.cell_size.cols,
                            visible_rows,
                        ),
                    );
                }
                projected.cell_size.rows = visible_rows;
                if projected.payload.raw.is_none() && projected.payload.pixels.is_some() {
                    match encode_projected_payload(&projected) {
                        Ok(raw) => projected.payload.raw = Some(raw),
                        Err(error) => return Some(Err(error)),
                    }
                }
                Some(Ok(projected))
            })
            .collect()
    }

    /// Remove placements whose final row is older than retained terminal history.
    /// Apply main-screen retention after a geometry change, including while
    /// the main screen is hidden. Alternate-screen placements are unaffected.
    pub fn evict_main_history(&mut self, retained_rows: usize) {
        if let Some(normal) = self.normal_screen.as_mut() {
            normal.evict_history(retained_rows);
        } else {
            self.evict_history(retained_rows);
        }
    }

    pub fn evict_history(&mut self, retained_rows: usize) {
        let oldest = i64::try_from(retained_rows)
            .unwrap_or(i64::MAX)
            .saturating_neg();
        let before = self.history.len();
        self.history
            .retain(|_, (image, row)| row.saturating_add(i64::from(image.cell_size.rows)) > oldest);
        if self.history.len() != before {
            self.sequence += 1;
        }
        #[cfg(feature = "kitty")]
        self.kitty_placements
            .retain(|_, id| self.history.contains_key(id));
    }

    /// Compute a delta since the given sequence number.
    pub fn delta_since(&self, since_sequence: u64) -> ImageDelta {
        if since_sequence == 0 || since_sequence >= self.sequence {
            // Either first request (full snapshot) or already up to date.
            if since_sequence == 0 {
                return ImageDelta {
                    added: self.images.clone(),
                    removed: Vec::new(),
                    sequence: self.sequence,
                };
            }
            return ImageDelta {
                added: Vec::new(),
                removed: Vec::new(),
                sequence: self.sequence,
            };
        }

        // Check if the change log covers the requested range.
        let oldest_log_seq = self
            .change_log
            .first()
            .map(|e| match e {
                ChangeLogEntry::Added { sequence, .. }
                | ChangeLogEntry::Removed { sequence, .. } => *sequence,
            })
            .unwrap_or(0);

        if since_sequence < oldest_log_seq {
            // Change log was compacted past the client's sequence.
            // Fall back to full snapshot.
            return ImageDelta {
                added: self.images.clone(),
                removed: Vec::new(),
                sequence: self.sequence,
            };
        }

        // A client may miss several updates, including placement and deletion
        // in the same PTY read. Return only the final state of each touched ID;
        // applying removals before additions must never resurrect an old image.
        let touched = self
            .change_log
            .iter()
            .filter_map(|entry| match entry {
                ChangeLogEntry::Added { sequence, image } if *sequence > since_sequence => {
                    Some(image.id)
                }
                ChangeLogEntry::Removed { sequence, image_id } if *sequence > since_sequence => {
                    Some(*image_id)
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut added = Vec::new();
        let mut removed = Vec::new();
        for id in touched {
            if let Some(image) = self.images.iter().find(|image| image.id == id) {
                // Consumers apply removals before additions. Include the ID
                // even when it survives so a moved/cropped placement replaces
                // its previous projection rather than accumulating duplicates.
                removed.push(id);
                added.push(image.clone());
            } else {
                removed.push(id);
            }
        }

        ImageDelta {
            added,
            removed,
            sequence: self.sequence,
        }
    }

    /// Current sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Clear the active display while preserving placements entirely in history.
    pub fn clear_display(&mut self) {
        let retained = self
            .history
            .iter()
            .filter(|(_, (image, row))| row.saturating_add(i64::from(image.cell_size.rows)) <= 0)
            .map(|(id, value)| (*id, value.clone()))
            .collect();
        self.sequence += 1;
        for image in &self.images {
            self.change_log.push(ChangeLogEntry::Removed {
                sequence: self.sequence,
                image_id: image.id,
            });
        }
        self.images.clear();
        self.history = retained;
        #[cfg(feature = "kitty")]
        self.kitty_placements
            .retain(|_, id| self.history.contains_key(id));
        self.compact_change_log();
    }

    /// Reset both screen namespaces without allowing hidden images to return.
    pub fn reset(&mut self) {
        self.clear();
        self.normal_screen = None;
    }

    /// Remove all images (e.g., on screen clear).
    pub fn clear(&mut self) {
        self.sequence += 1;
        for img in &self.images {
            self.change_log.push(ChangeLogEntry::Removed {
                sequence: self.sequence,
                image_id: img.id,
            });
        }
        self.images.clear();
        self.history.clear();
        self.compact_change_log();
        #[cfg(feature = "kitty")]
        {
            self.kitty_transmitted.clear();
            self.kitty_placements.clear();
            self.kitty_pending_chunks.clear();
        }
    }

    #[cfg(feature = "kitty")]
    fn remove_image(&mut self, id: u64) {
        let removed_history = self.history.remove(&id).is_some();
        self.kitty_placements
            .retain(|_, retained_id| *retained_id != id);
        let before = self.images.len();
        self.images.retain(|image| image.id != id);
        if self.images.len() != before || removed_history {
            self.sequence += 1;
            self.change_log.push(ChangeLogEntry::Removed {
                sequence: self.sequence,
                image_id: id,
            });
            self.compact_change_log();
        }
    }

    /// Handle a kitty graphics command.
    #[cfg(feature = "kitty")]
    fn handle_kitty_command(
        &mut self,
        cmd: crate::model::KittyCommand,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) {
        use crate::model::{KittyCommand, KittyDeleteSpecifier, KittyTransmittedImage};

        match cmd {
            KittyCommand::Transmit {
                image_id,
                format,
                data,
                width,
                height,
                more_chunks: false,
            } => {
                // Check if this is the final chunk of a multi-chunk transmission.
                let (final_data, format, width, height) =
                    if let Some(mut acc) = self.kitty_pending_chunks.remove(&image_id) {
                        if self.max_bytes > 0
                            && acc.data.len().saturating_add(data.len()) > self.max_bytes
                        {
                            return;
                        }
                        acc.data.extend_from_slice(&data);
                        (acc.data, acc.format, acc.width, acc.height)
                    } else {
                        if self.max_bytes > 0 && data.len() > self.max_bytes {
                            return;
                        }
                        (data, format, width, height)
                    };
                self.kitty_transmitted.insert(
                    image_id,
                    KittyTransmittedImage {
                        image_id,
                        format,
                        data: final_data,
                        width,
                        height,
                    },
                );
                self.sequence += 1;
            }
            KittyCommand::Transmit {
                image_id,
                format,
                data,
                width,
                height,
                more_chunks: true,
            } => {
                // Accumulate chunks until the final chunk arrives.
                let acc = self
                    .kitty_pending_chunks
                    .entry(image_id)
                    .or_insert_with(|| KittyChunkAccumulator {
                        data: Vec::new(),
                        format,
                        width,
                        height,
                    });
                if self.max_bytes > 0 && acc.data.len().saturating_add(data.len()) > self.max_bytes
                {
                    self.kitty_pending_chunks.remove(&image_id);
                    return;
                }
                acc.data.extend_from_slice(&data);
            }
            KittyCommand::Place(placement) => {
                // If we have the transmitted image, create a PaneImage.
                if let Some(transmitted) = self.kitty_transmitted.get(&placement.image_id) {
                    let pixel_size = ImagePixelSize {
                        width: transmitted.width,
                        height: transmitted.height,
                    };
                    let mut cell_size =
                        pixel_size_to_cells(pixel_size, cell_pixel_width, cell_pixel_height);
                    if placement.cell_size.cols > 0 {
                        cell_size.cols = placement.cell_size.cols;
                    }
                    if placement.cell_size.rows > 0 {
                        cell_size.rows = placement.cell_size.rows;
                    }
                    // Preserve the transmitted format rather than treating raw
                    // RGB/RGBA bytes as an encoded PNG during host rendering.
                    let format = match transmitted.format {
                        crate::model::KittyFormat::Rgb => crate::model::PixelFormat::Rgb8,
                        crate::model::KittyFormat::Rgba => crate::model::PixelFormat::Rgba8,
                        crate::model::KittyFormat::Png => crate::model::PixelFormat::Png,
                    };
                    let pixels = crate::model::PixelBuffer {
                        data: transmitted.data.clone(),
                        width: transmitted.width,
                        height: transmitted.height,
                        format,
                    };
                    // The existing attach representation carries Kitty PNG
                    // payloads. Normalize here without changing the wire format.
                    let raw = if format == crate::model::PixelFormat::Png {
                        pixels.data.clone()
                    } else {
                        use image::ImageEncoder;
                        let color = if format == crate::model::PixelFormat::Rgb8 {
                            image::ExtendedColorType::Rgb8
                        } else {
                            image::ExtendedColorType::Rgba8
                        };
                        let expected = u64::from(pixels.width)
                            .checked_mul(u64::from(pixels.height))
                            .and_then(|size| size.checked_mul(u64::from(color.channel_count())));
                        if expected != Some(pixels.data.len() as u64) {
                            return;
                        }
                        let mut png = Vec::new();
                        if image::codecs::png::PngEncoder::new(&mut png)
                            .write_image(&pixels.data, pixels.width, pixels.height, color)
                            .is_err()
                        {
                            return;
                        }
                        png
                    };
                    let key = (placement.image_id, placement.placement_id);
                    if let Some(old_id) = self.kitty_placements.remove(&key) {
                        self.remove_image(old_id);
                    }
                    let retained_id = self.next_id;
                    self.add_image(
                        ImageProtocol::KittyGraphics,
                        ImagePayload {
                            raw: Some(raw),
                            pixels: Some(pixels),
                        },
                        placement.position,
                        cell_size,
                        pixel_size,
                    );
                    if self.images.iter().any(|image| image.id == retained_id) {
                        self.kitty_placements.insert(key, retained_id);
                    }
                }
            }
            KittyCommand::Delete { specifier } => match specifier {
                KittyDeleteSpecifier::All => {
                    let ids = self.kitty_placements.values().copied().collect::<Vec<_>>();
                    for id in ids {
                        self.remove_image(id);
                    }
                }
                KittyDeleteSpecifier::ByImageId(id) => {
                    self.kitty_transmitted.remove(&id);
                    self.kitty_pending_chunks.remove(&id);
                    let ids = self
                        .kitty_placements
                        .iter()
                        .filter_map(|(&(image_id, _), &retained_id)| {
                            (image_id == id).then_some(retained_id)
                        })
                        .collect::<Vec<_>>();
                    for retained_id in ids {
                        self.remove_image(retained_id);
                    }
                }
                KittyDeleteSpecifier::ByPlacementId {
                    image_id,
                    placement_id,
                } => {
                    if let Some(retained_id) =
                        self.kitty_placements.remove(&(image_id, placement_id))
                    {
                        self.remove_image(retained_id);
                    }
                }
            },
            KittyCommand::Query { .. } => {
                // Queries are forwarded, not stored.
            }
        }
    }
}

/// Preserve the existing protocol payload representation for a cropped view.
fn encode_projected_payload(image: &PaneImage) -> std::io::Result<Vec<u8>> {
    use image::ImageEncoder;
    let pixels = image
        .payload
        .pixels
        .as_ref()
        .ok_or_else(|| std::io::Error::other("projected image has no pixels"))?;
    #[cfg(feature = "sixel")]
    if image.protocol == ImageProtocol::Sixel {
        return crate::codec::sixel::encode(pixels)
            .ok_or_else(|| std::io::Error::other("could not encode projected Sixel image"));
    }
    let color = match pixels.format {
        crate::model::PixelFormat::Rgb8 => image::ExtendedColorType::Rgb8,
        crate::model::PixelFormat::Rgba8 => image::ExtendedColorType::Rgba8,
        crate::model::PixelFormat::Png => {
            return Err(std::io::Error::other("projection requires decoded pixels"));
        }
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels.data, pixels.width, pixels.height, color)
        .map_err(std::io::Error::other)?;
    match image.protocol {
        ImageProtocol::KittyGraphics => Ok(png),
        #[cfg(feature = "iterm2")]
        ImageProtocol::ITerm2 => Ok(crate::codec::iterm2::encode_body_with_cells(
            &png,
            image.cell_size.cols,
            image.cell_size.rows,
        )),
        _ => Err(std::io::Error::other(
            "projected image protocol is not enabled",
        )),
    }
}

impl Default for ImageRegistry {
    fn default() -> Self {
        Self::new(100, 10 * 1024 * 1024) // 100 images, 10 MiB
    }
}

/// Convert pixel dimensions to cell dimensions.
#[allow(dead_code)] // Called from feature-gated image processing paths
fn pixel_size_to_cells(
    pixel_size: ImagePixelSize,
    cell_pixel_width: u16,
    cell_pixel_height: u16,
) -> ImageCellSize {
    if cell_pixel_width == 0 || cell_pixel_height == 0 {
        return ImageCellSize { rows: 1, cols: 1 };
    }
    ImageCellSize {
        rows: (pixel_size.height as u16).div_ceil(cell_pixel_height),
        cols: (pixel_size.width as u16).div_ceil(cell_pixel_width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternate_resize_updates_empty_hidden_screen_geometry() {
        for (initial, resized) in [(3, 6), (6, 3)] {
            let mut registry = ImageRegistry::default().with_viewport_height(initial);
            registry.set_alternate_screen(true);
            registry.resize_alternate_viewport(resized).unwrap();
            registry.set_alternate_screen(false);
            assert_eq!(registry.viewport_height, resized);
            registry.add_image(
                ImageProtocol::Sixel,
                ImagePayload::default(),
                ImagePosition { row: 4, col: 0 },
                ImageCellSize { rows: 1, cols: 1 },
                ImagePixelSize {
                    width: 8,
                    height: 16,
                },
            );
            assert_eq!(registry.images().len(), usize::from(resized > 4));
        }
    }

    #[test]
    fn mapped_projection_skips_invisible_payload_before_cropping() {
        let mut registry = ImageRegistry::default();
        registry.add_image(
            ImageProtocol::Sixel,
            ImagePayload::default(),
            ImagePosition { row: 0, col: 10 },
            ImageCellSize { rows: 2, cols: 2 },
            ImagePixelSize {
                width: 16,
                height: 32,
            },
        );
        let snapshot = registry.capture_history(&mut 100_000).unwrap();
        let projected = snapshot
            .project_mapped(5, 1, &mut 100_000, |row, col| Ok((row, col)))
            .unwrap();
        assert!(projected.is_empty());
        // The same payload must still fail if it is visible and needs cropping.
        assert!(
            snapshot
                .project_mapped(20, 1, &mut 100_000, |row, col| Ok((row, col)))
                .is_err()
        );
    }

    #[test]
    fn empty_viewport_does_not_decode_a_straddling_image() {
        let mut registry = ImageRegistry::default();
        registry.add_image(
            ImageProtocol::Sixel,
            ImagePayload::default(),
            ImagePosition { row: 0, col: 0 },
            ImageCellSize { rows: 2, cols: 1 },
            ImagePixelSize {
                width: 8,
                height: 32,
            },
        );
        // Retain an undecodable placement crossing the top edge. Any nonempty
        // viewport would need to crop it and must report its invalid payload.
        registry.history.values_mut().next().unwrap().1 = -1;
        assert!(registry.project_viewport(0, 0).unwrap().is_empty());
        assert!(registry.project_viewport(0, 1).is_err());
    }

    #[test]
    fn empty_mapped_viewport_neither_allocates_nor_maps_anchors() {
        let mut registry = ImageRegistry::default();
        registry.add_image(
            ImageProtocol::Sixel,
            ImagePayload::default(),
            ImagePosition { row: 0, col: 0 },
            ImageCellSize { rows: 1, cols: 1 },
            ImagePixelSize {
                width: 8,
                height: 16,
            },
        );
        let snapshot = registry.capture_history(&mut 100_000).unwrap();
        for (width, height) in [(0, 1), (1, 0), (0, 0)] {
            let mut budget = 0;
            let images = snapshot
                .project_mapped(width, height, &mut budget, |_, _| {
                    panic!("empty window must not map anchors")
                })
                .unwrap();
            assert!(images.is_empty());
            assert_eq!(budget, 0);
        }
    }

    #[test]
    fn pixel_size_to_cells_rounds_up() {
        let size = pixel_size_to_cells(
            ImagePixelSize {
                width: 100,
                height: 50,
            },
            8,
            16,
        );
        // 100/8 = 12.5 -> 13 cols, 50/16 = 3.125 -> 4 rows
        assert_eq!(size.cols, 13);
        assert_eq!(size.rows, 4);
    }

    #[test]
    fn registry_enforces_max_images() {
        let mut reg = ImageRegistry::new(2, 0);
        for i in 0..5 {
            reg.add_image(
                ImageProtocol::Sixel,
                ImagePayload::default(),
                ImagePosition { row: i, col: 0 },
                ImageCellSize { rows: 1, cols: 1 },
                ImagePixelSize {
                    width: 10,
                    height: 10,
                },
            );
        }
        assert_eq!(reg.images().len(), 2);
        // Oldest images were evicted; newest two remain.
        assert_eq!(reg.images()[0].position.row, 3);
        assert_eq!(reg.images()[1].position.row, 4);
    }

    #[cfg(feature = "kitty")]
    #[test]
    fn deleting_image_discards_pending_transmission() {
        use crate::model::{KittyCommand, KittyDeleteSpecifier, KittyFormat};

        let mut reg = ImageRegistry::default();
        reg.handle_kitty_command(
            KittyCommand::Transmit {
                image_id: 42,
                format: KittyFormat::Rgba,
                data: vec![1, 2],
                width: 1,
                height: 1,
                more_chunks: true,
            },
            8,
            16,
        );
        reg.handle_kitty_command(
            KittyCommand::Delete {
                specifier: KittyDeleteSpecifier::ByImageId(42),
            },
            8,
            16,
        );
        assert!(!reg.kitty_pending_chunks.contains_key(&42));
        reg.handle_kitty_command(
            KittyCommand::Transmit {
                image_id: 42,
                format: KittyFormat::Rgba,
                data: vec![3, 4, 5, 6],
                width: 1,
                height: 1,
                more_chunks: false,
            },
            8,
            16,
        );
        assert_eq!(reg.kitty_transmitted[&42].data, vec![3, 4, 5, 6]);
    }

    #[test]
    fn failed_scroll_restores_saturated_history_positions() {
        let mut reg = ImageRegistry::new(10, 0);
        for row in [0, 1] {
            reg.add_image(
                ImageProtocol::Sixel,
                ImagePayload::default(),
                ImagePosition { row, col: 0 },
                ImageCellSize { rows: 3, cols: 1 },
                ImagePixelSize {
                    width: 8,
                    height: 48,
                },
            );
        }
        reg.history.values_mut().next().unwrap().1 = i64::MIN;
        let rows = reg
            .history
            .values()
            .map(|(_, row)| *row)
            .collect::<Vec<_>>();
        let images = reg.images().to_vec();
        let revision = reg.sequence();

        // The second image needs cropping, but has no decodable payload.
        assert!(reg.scroll_up(2).is_err());
        assert_eq!(
            reg.history
                .values()
                .map(|(_, row)| *row)
                .collect::<Vec<_>>(),
            rows
        );
        assert_eq!(reg.images(), images);
        assert_eq!(reg.sequence(), revision);
    }

    #[test]
    fn scroll_up_shifts_and_evicts() {
        let mut reg = ImageRegistry::new(10, 0);
        reg.add_image(
            ImageProtocol::Sixel,
            ImagePayload::default(),
            ImagePosition { row: 0, col: 0 },
            ImageCellSize { rows: 2, cols: 5 },
            ImagePixelSize {
                width: 40,
                height: 32,
            },
        );
        reg.add_image(
            ImageProtocol::Sixel,
            ImagePayload::default(),
            ImagePosition { row: 5, col: 0 },
            ImageCellSize { rows: 1, cols: 5 },
            ImagePixelSize {
                width: 40,
                height: 16,
            },
        );

        reg.scroll_up(3).unwrap();

        // First image (row 0, height 2) scrolled above row 3 entirely -> removed.
        // Second image (row 5) -> row 2.
        assert_eq!(reg.images().len(), 1);
        assert_eq!(reg.images()[0].position.row, 2);
    }

    #[test]
    fn scroll_up_partial_clip_preserves_visible_rows() {
        let mut reg = ImageRegistry::new(10, 0);
        // Image at row 2, spanning 5 rows (rows 2..7).
        reg.add_image(
            ImageProtocol::KittyGraphics,
            ImagePayload {
                raw: None,
                pixels: Some(crate::model::PixelBuffer {
                    width: 40,
                    height: 80,
                    format: crate::model::PixelFormat::Rgba8,
                    data: vec![255; 40 * 80 * 4],
                }),
            },
            ImagePosition { row: 2, col: 0 },
            ImageCellSize { rows: 5, cols: 5 },
            ImagePixelSize {
                width: 40,
                height: 80,
            },
        );

        // Scroll up by 4: rows 0..4 disappear. The image originally at rows
        // 2..7 loses its top 2 rows (rows 2..4) and keeps the bottom 3 (rows
        // 4..7, now shifted to 0..3).
        reg.scroll_up(4).unwrap();

        assert_eq!(reg.images().len(), 1);
        let img = &reg.images()[0];
        assert_eq!(
            img.position.row, 0,
            "partially clipped image should be at row 0"
        );
        assert_eq!(
            img.cell_size.rows, 3,
            "image should keep 3 visible rows (original 5 minus 2 clipped)"
        );
    }
}
