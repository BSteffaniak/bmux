//! Per-pane image registry.
//!
//! Stores active images, tracks scroll-based position shifts, evicts
//! images that scroll past the scrollback limit, and provides delta
//! queries for efficient IPC transport.

use crate::model::{
    ImageCellSize, ImageDelta, ImageEvent, ImagePayload, ImagePixelSize, ImagePosition,
    ImageProtocol, PaneImage,
};

/// Per-pane image storage with scroll tracking and delta queries.
pub struct ImageRegistry {
    images: Vec<PaneImage>,
    next_id: u64,
    /// Monotonic sequence counter; incremented on every mutation.
    sequence: u64,
    /// Maximum number of images kept per pane.
    max_images: usize,
    /// Maximum bytes of image payload per pane (0 = unlimited).
    max_bytes: usize,

    // Kitty-specific state
    #[cfg(feature = "kitty")]
    kitty_transmitted: std::collections::BTreeMap<u32, crate::model::KittyTransmittedImage>,
    #[cfg(feature = "kitty")]
    kitty_placements: Vec<crate::model::KittyPlacement>,
}

impl ImageRegistry {
    /// Create a new empty registry.
    pub fn new(max_images: usize, max_bytes: usize) -> Self {
        Self {
            images: Vec::new(),
            next_id: 1,
            sequence: 0,
            max_images,
            max_bytes,
            #[cfg(feature = "kitty")]
            kitty_transmitted: std::collections::BTreeMap::new(),
            #[cfg(feature = "kitty")]
            kitty_placements: Vec::new(),
        }
    }

    /// Handle an image event produced by the interceptor.
    ///
    /// `cell_width` and `cell_height` are the pane's cell dimensions in
    /// pixels, used to compute `cell_size` from pixel dimensions.
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
            ImageEvent::KittyCommand(cmd) => {
                self.handle_kitty_command(cmd, cell_pixel_width, cell_pixel_height);
            }

            #[cfg(feature = "iterm2")]
            ImageEvent::ITerm2Image { data, position } => {
                // iTerm2 images are base64-encoded; we don't know pixel size
                // until decoding. Store with a placeholder size.
                let pixel_size = ImagePixelSize {
                    width: 0,
                    height: 0,
                };
                let cell_size = ImageCellSize { rows: 1, cols: 1 };
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

        self.images.push(PaneImage {
            id,
            protocol,
            payload,
            position,
            cell_size,
            pixel_size,
        });

        self.enforce_limits();
    }

    /// Remove images exceeding the per-pane limits (oldest first).
    fn enforce_limits(&mut self) {
        while self.images.len() > self.max_images {
            self.images.remove(0);
            self.sequence += 1;
        }

        if self.max_bytes > 0 {
            while self.total_bytes() > self.max_bytes && !self.images.is_empty() {
                self.images.remove(0);
                self.sequence += 1;
            }
        }
    }

    fn total_bytes(&self) -> usize {
        self.images
            .iter()
            .map(|img| {
                img.payload.raw.as_ref().map_or(0, |r| r.len())
                    + img.payload.pixels.as_ref().map_or(0, |p| p.data.len())
            })
            .sum()
    }

    /// Shift all image positions up when the pane scrolls.
    ///
    /// Call this when the pane's content scrolls by `lines` rows.
    /// Images that scroll entirely above the viewport are removed.
    pub fn scroll_up(&mut self, lines: u16) {
        self.sequence += 1;
        self.images.retain_mut(|img| {
            if img.position.row < lines {
                // Image scrolled above the visible area.
                // Keep if part of the image is still visible.
                if img.position.row + img.cell_size.rows > lines {
                    img.position.row = 0;
                    img.cell_size.rows -= lines - img.position.row;
                    true
                } else {
                    false
                }
            } else {
                img.position.row -= lines;
                true
            }
        });
    }

    /// Get all images currently in the registry.
    pub fn images(&self) -> &[PaneImage] {
        &self.images
    }

    /// Get images visible within a viewport of `height` rows starting at
    /// scrollback `offset` (0 = bottom/live).
    pub fn images_in_viewport(&self, _offset: usize, height: u16) -> Vec<&PaneImage> {
        self.images
            .iter()
            .filter(|img| img.position.row < height)
            .collect()
    }

    /// Compute a delta since the given sequence number.
    pub fn delta_since(&self, since_sequence: u64) -> ImageDelta {
        if since_sequence == 0 {
            // Full snapshot.
            return ImageDelta {
                added: self.images.clone(),
                removed: Vec::new(),
                sequence: self.sequence,
            };
        }

        // For now, always send a full snapshot.  A proper delta tracker
        // would record removed IDs and only send new images.
        // TODO: implement proper delta tracking with a change log.
        ImageDelta {
            added: self.images.clone(),
            removed: Vec::new(),
            sequence: self.sequence,
        }
    }

    /// Current sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Remove all images (e.g., on screen clear).
    pub fn clear(&mut self) {
        self.images.clear();
        self.sequence += 1;
        #[cfg(feature = "kitty")]
        {
            self.kitty_transmitted.clear();
            self.kitty_placements.clear();
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
                self.kitty_transmitted.insert(
                    image_id,
                    KittyTransmittedImage {
                        image_id,
                        format,
                        data,
                        width,
                        height,
                    },
                );
                self.sequence += 1;
            }
            KittyCommand::Transmit {
                more_chunks: true, ..
            } => {
                // TODO: handle chunked transmission — accumulate data until
                // the final chunk (more_chunks=false) arrives.
            }
            KittyCommand::Place(placement) => {
                // If we have the transmitted image, create a PaneImage.
                if let Some(transmitted) = self.kitty_transmitted.get(&placement.image_id) {
                    let pixel_size = ImagePixelSize {
                        width: transmitted.width,
                        height: transmitted.height,
                    };
                    let cell_size =
                        pixel_size_to_cells(pixel_size, cell_pixel_width, cell_pixel_height);
                    self.add_image(
                        ImageProtocol::KittyGraphics,
                        ImagePayload {
                            raw: Some(transmitted.data.clone()),
                            pixels: None,
                        },
                        placement.position,
                        cell_size,
                        pixel_size,
                    );
                }
                self.kitty_placements.push(placement);
            }
            KittyCommand::Delete { specifier } => {
                match specifier {
                    KittyDeleteSpecifier::All => self.clear(),
                    KittyDeleteSpecifier::ByImageId(id) => {
                        self.kitty_transmitted.remove(&id);
                        self.kitty_placements.retain(|p| p.image_id != id);
                        // Also remove rendered PaneImages from this kitty image.
                        // For now, we don't track which PaneImage came from which
                        // kitty image_id, so this is a TODO.
                        self.sequence += 1;
                    }
                    KittyDeleteSpecifier::ByPlacementId {
                        image_id,
                        placement_id,
                    } => {
                        self.kitty_placements.retain(|p| {
                            !(p.image_id == image_id && p.placement_id == placement_id)
                        });
                        self.sequence += 1;
                    }
                }
            }
            KittyCommand::Query { .. } => {
                // Queries are forwarded, not stored.
            }
        }
    }
}

impl Default for ImageRegistry {
    fn default() -> Self {
        Self::new(100, 10 * 1024 * 1024) // 100 images, 10 MiB
    }
}

/// Convert pixel dimensions to cell dimensions.
fn pixel_size_to_cells(
    pixel_size: ImagePixelSize,
    cell_pixel_width: u16,
    cell_pixel_height: u16,
) -> ImageCellSize {
    if cell_pixel_width == 0 || cell_pixel_height == 0 {
        return ImageCellSize { rows: 1, cols: 1 };
    }
    ImageCellSize {
        rows: ((pixel_size.height as u16).saturating_add(cell_pixel_height - 1))
            / cell_pixel_height,
        cols: ((pixel_size.width as u16).saturating_add(cell_pixel_width - 1)) / cell_pixel_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        reg.scroll_up(3);

        // First image (row 0, height 2) scrolled above row 3 entirely -> removed.
        // Second image (row 5) -> row 2.
        assert_eq!(reg.images().len(), 1);
        assert_eq!(reg.images()[0].position.row, 2);
    }
}
