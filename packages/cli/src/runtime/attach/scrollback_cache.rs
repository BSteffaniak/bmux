//! Bounded, reconstructible scrollback projections owned by one attachment.

use bmux_attach_pipeline::{PaneScrollbackWindow, ScrollbackPin};
use uuid::Uuid;

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_WINDOWS: usize = 32;

struct Entry {
    pane: Uuid,
    pin: Option<ScrollbackPin>,
    bytes: usize,
    window: PaneScrollbackWindow,
}

#[derive(Default)]
pub(super) struct ScrollbackCache {
    entries: std::collections::VecDeque<Entry>,
    bytes: usize,
}

pub fn project_images(
    images: &[bmux_attach_image_protocol::AttachPaneImage],
    start: usize,
    rows: usize,
) -> Option<Vec<bmux_attach_image_protocol::AttachPaneImage>> {
    if images.is_empty() {
        return Some(Vec::new());
    }
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    {
        let images = images
            .iter()
            .map(bmux_image::PaneImage::from)
            .collect::<Vec<_>>();
        bmux_image::ImageRegistry::project_row_slice(
            &images,
            u16::try_from(start).ok()?,
            u16::try_from(rows).ok()?,
        )
        .ok()
        .map(|images| {
            images
                .iter()
                .map(bmux_attach_image_protocol::AttachPaneImage::from)
                .collect()
        })
    }
    #[cfg(not(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    )))]
    {
        let _ = (start, rows);
        None
    }
}

impl ScrollbackCache {
    pub fn advance(&mut self, pane: Uuid, previous: u64, current: u64) {
        let Some(growth) = current
            .checked_sub(previous)
            .and_then(|n| usize::try_from(n).ok())
        else {
            self.invalidate(pane);
            return;
        };
        for entry in &mut self.entries {
            if entry.pane == pane
                && entry.pin.is_none()
                && entry.window.total_scrolled_rows == previous
            {
                entry.window.total_scrolled_rows = current;
                entry.window.scrollback_offset =
                    entry.window.scrollback_offset.saturating_add(growth);
                entry.window.max_scrollback_offset =
                    entry.window.max_scrollback_offset.saturating_add(growth);
            }
        }
    }

    pub fn invalidate(&mut self, pane: Uuid) {
        self.entries.retain(|entry| {
            if entry.pane == pane {
                self.bytes -= entry.bytes;
                false
            } else {
                true
            }
        });
    }

    pub fn get(
        &self,
        pane: Uuid,
        pin: Option<ScrollbackPin>,
        offset: usize,
        width: usize,
        rows: usize,
    ) -> Option<PaneScrollbackWindow> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| {
                let window = &entry.window;
                if entry.pane != pane
                    || entry.pin != pin
                    || window.projection_width != width
                    || offset > window.max_scrollback_offset
                {
                    return None;
                }
                let shift = offset.checked_sub(window.scrollback_offset)?;
                let end = window.rows.len().checked_sub(shift)?;
                let start = end.checked_sub(rows)?;
                let images = if start == 0 && end == window.rows.len() {
                    window.images.clone()
                } else {
                    project_images(&window.images, start, rows)?
                };
                Some(PaneScrollbackWindow {
                    images,
                    projection_width: width,
                    row_anchors: if window.row_anchors.is_empty() {
                        Vec::new()
                    } else {
                        window.row_anchors.get(start..end)?.to_vec()
                    },
                    palette: window.palette.clone(),
                    scrollback_offset: offset,
                    max_scrollback_offset: window.max_scrollback_offset,
                    total_scrolled_rows: window.total_scrolled_rows,
                    rows: window.rows[start..end].to_vec(),
                })
            })
            .or_else(|| self.assemble_resident(pane, pin, offset, width, rows))
    }

    /// Assemble overlapping text-only projections without transport. Only one
    /// source epoch and palette may contribute; image-bearing ranges keep the
    /// existing coherent-window path until image coverage is range-addressable.
    fn assemble_resident(
        &self,
        pane: Uuid,
        pin: Option<ScrollbackPin>,
        offset: usize,
        width: usize,
        rows: usize,
    ) -> Option<PaneScrollbackWindow> {
        if rows == 0 || rows > 256 {
            return None;
        }
        let reference = self.entries.iter().rev().find(|entry| {
            entry.pane == pane
                && entry.pin == pin
                && entry.window.projection_width == width
                && offset <= entry.window.max_scrollback_offset
        })?;
        let base = &reference.window;
        let captured = !base.row_anchors.is_empty();
        let mut output = Vec::with_capacity(rows);
        let mut anchors = Vec::with_capacity(if captured { rows } else { 0 });
        for row in 0..rows {
            let distance = offset.checked_add(rows - row - 1)?;
            let (window, index) = self.entries.iter().rev().find_map(|entry| {
                let window = &entry.window;
                if entry.pane != pane
                    || entry.pin != pin
                    || window.projection_width != width
                    || window.total_scrolled_rows != base.total_scrolled_rows
                    || window.palette.styles() != base.palette.styles()
                    || !window.images.is_empty()
                    || window.row_anchors.is_empty() == captured
                {
                    return None;
                }
                let shift = distance.checked_sub(window.scrollback_offset)?;
                let index = window.rows.len().checked_sub(shift.checked_add(1)?)?;
                Some((window, index))
            })?;
            output.push(window.rows[index].clone());
            if captured {
                anchors.push(*window.row_anchors.get(index)?);
            }
        }
        Some(PaneScrollbackWindow {
            images: Vec::new(),
            projection_width: width,
            row_anchors: anchors,
            palette: base.palette.clone(),
            scrollback_offset: offset,
            max_scrollback_offset: base.max_scrollback_offset,
            total_scrolled_rows: base.total_scrolled_rows,
            rows: output,
        })
    }

    pub fn insert(
        &mut self,
        pane: Uuid,
        pin: Option<ScrollbackPin>,
        source: &PaneScrollbackWindow,
    ) {
        // Do not duplicate a projection when publishing a cache hit.
        if self.entries.iter().any(|entry| {
            entry.pane == pane
                && entry.pin == pin
                && entry.window.projection_width == source.projection_width
                && entry.window.scrollback_offset == source.scrollback_offset
                && entry.window.total_scrolled_rows == source.total_scrolled_rows
                && entry.window.rows.len() == source.rows.len()
        }) {
            return;
        }
        let image_bytes = source.images.iter().fold(0usize, |bytes, image| {
            bytes
                .saturating_add(std::mem::size_of_val(image))
                .saturating_add(image.raw_data.len())
        });
        if image_bytes > MAX_BYTES {
            return;
        }
        let window = PaneScrollbackWindow {
            images: source.images.clone(),
            projection_width: source.projection_width,
            row_anchors: source.row_anchors.clone(),
            palette: source.palette.clone(),
            scrollback_offset: source.scrollback_offset,
            max_scrollback_offset: source.max_scrollback_offset,
            total_scrolled_rows: source.total_scrolled_rows,
            rows: source.rows.clone(),
        };
        // Serialization accounts for variable-sized text and style payloads;
        // also charge the retained in-memory cell/row representation.
        let Ok(palette) = serde_json::to_vec(window.palette.styles()) else {
            return;
        };
        let bytes = window.rows.iter().fold(
            palette
                .len()
                .saturating_add(image_bytes)
                .saturating_add(std::mem::size_of::<Entry>())
                .saturating_add(
                    window.rows.capacity() * std::mem::size_of::<bmux_terminal_grid::PhysicalRow>(),
                )
                .saturating_add(
                    window.row_anchors.capacity()
                        * std::mem::size_of::<bmux_attach_pipeline::CapturedHistoryAnchor>(),
                ),
            |bytes, row| {
                row.cells().iter().fold(bytes, |bytes, cell| {
                    bytes
                        .saturating_add(std::mem::size_of::<bmux_terminal_grid::Cell>())
                        .saturating_add(cell.text().len())
                })
            },
        );
        if bytes > MAX_BYTES {
            return;
        }
        while self.bytes.saturating_add(bytes) > MAX_BYTES || self.entries.len() >= MAX_WINDOWS {
            if let Some(entry) = self.entries.pop_front() {
                self.bytes -= entry.bytes;
            }
        }
        self.bytes += bytes;
        self.entries.push_back(Entry {
            pane,
            pin,
            bytes,
            window,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> PaneScrollbackWindow {
        PaneScrollbackWindow {
            images: Vec::new(),
            projection_width: 80,
            row_anchors: Vec::new(),
            palette: bmux_terminal_grid::StylePalette::default(),
            scrollback_offset: 10,
            max_scrollback_offset: 100,
            total_scrolled_rows: 100,
            rows: vec![bmux_terminal_grid::PhysicalRow::default(); 40],
        }
    }

    #[test]
    fn captured_images_project_locally_and_count_toward_budget() {
        let pane = Uuid::new_v4();
        let mut source = window();
        source.row_anchors = vec![
            bmux_attach_pipeline::CapturedHistoryAnchor {
                capture_id: Uuid::new_v4(),
                line_index: 0,
                column: 0,
            };
            source.rows.len()
        ];
        source
            .images
            .push(bmux_attach_image_protocol::AttachPaneImage {
                id: 1,
                protocol: bmux_attach_image_protocol::AttachImageProtocol::Sixel,
                compression: bmux_attach_image_protocol::CompressionId::None,
                raw_data: vec![1, 2, 3],
                position_row: 2,
                position_col: 3,
                cell_rows: 1,
                cell_cols: 1,
                pixel_width: 1,
                pixel_height: 1,
            });
        let mut cache = ScrollbackCache::default();
        cache.insert(pane, None, &source);
        let hit = cache.get(pane, None, 10, 80, 40).unwrap();
        assert_eq!(hit.images, source.images);
        #[cfg(any(
            feature = "image-sixel",
            feature = "image-kitty",
            feature = "image-iterm2"
        ))]
        {
            let shifted = cache.get(pane, None, 11, 80, 38).unwrap();
            assert_eq!(shifted.images[0].position_row, 1);
            assert_eq!(shifted.images[0].raw_data, source.images[0].raw_data);
            assert!(cache.get(pane, None, 10, 80, 20).unwrap().images.is_empty());
            // Projection never mutates the retained source on direction reversal.
            assert_eq!(
                cache.get(pane, None, 10, 80, 40).unwrap().images,
                source.images
            );
        }
        assert!(cache.get(pane, None, 10, 40, 40).is_none());
        cache.invalidate(pane);
        source.images[0].raw_data = vec![0; MAX_BYTES + 1];
        cache.insert(pane, None, &source);
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn overlapping_navigation_is_local_and_bounded() {
        let pane = Uuid::new_v4();
        let mut cache = ScrollbackCache::default();
        cache.insert(pane, None, &window());
        for offset in 10..=30 {
            let hit = cache.get(pane, None, offset, 80, 20).unwrap();
            assert_eq!(hit.rows.len(), 20);
            assert_eq!(hit.scrollback_offset, offset);
        }
        assert!(cache.get(pane, None, 31, 80, 20).is_none());
        assert!(cache.get(pane, None, 9, 80, 20).is_none());
        assert!(cache.get(pane, None, 10, 79, 20).is_none());
        assert!(cache.get(Uuid::new_v4(), None, 10, 80, 20).is_none());
    }

    #[test]
    fn overlapping_windows_cover_intermediate_viewports_without_a_fetch() {
        let pane = Uuid::new_v4();
        let mut cache = ScrollbackCache::default();
        let mut first = window();
        first.rows.truncate(20);
        cache.insert(pane, None, &first);
        let mut second = window();
        second.rows.truncate(20);
        second.scrollback_offset = 20;
        cache.insert(pane, None, &second);
        for offset in 10..=20 {
            let view = cache.get(pane, None, offset, 80, 20).unwrap();
            assert_eq!(view.rows.len(), 20);
            assert_eq!(view.scrollback_offset, offset);
        }
        // Never combine content from incompatible source numbering.
        cache.entries.back_mut().unwrap().window.total_scrolled_rows += 1;
        assert!(cache.get(pane, None, 15, 80, 20).is_none());
    }

    #[test]
    fn live_growth_rebases_cached_rows_without_discarding_them() {
        let pane = Uuid::new_v4();
        let mut cache = ScrollbackCache::default();
        cache.insert(pane, None, &window());
        cache.advance(pane, 100, 110);
        let hit = cache.get(pane, None, 20, 80, 20).unwrap();
        assert_eq!(hit.total_scrolled_rows, 110);
        assert_eq!(hit.total_scrolled_rows - hit.scrollback_offset as u64, 90);
        assert!(cache.get(pane, None, 10, 80, 20).is_none());
        cache.advance(pane, 110, 1);
        assert!(cache.get(pane, None, 20, 80, 20).is_none());
    }

    #[test]
    fn invalidation_and_eviction_drop_old_projections() {
        let mut cache = ScrollbackCache::default();
        let first = Uuid::new_v4();
        cache.insert(first, None, &window());
        cache.insert(first, None, &window());
        assert_eq!(cache.entries.len(), 1);
        for _ in 0..MAX_WINDOWS {
            cache.insert(Uuid::new_v4(), None, &window());
        }
        assert_eq!(cache.entries.len(), MAX_WINDOWS);
        assert!(cache.bytes <= MAX_BYTES);
        assert!(cache.get(first, None, 10, 80, 20).is_none());
        let pane = cache.entries.back().unwrap().pane;
        cache.invalidate(pane);
        assert!(cache.get(pane, None, 10, 80, 20).is_none());
        assert_eq!(
            cache.bytes,
            cache.entries.iter().map(|entry| entry.bytes).sum::<usize>()
        );
    }
}
