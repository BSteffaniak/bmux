//! Bundled attach-local visual adapters.
//!
//! This crate intentionally lives outside core. It registers common adapter
//! implementations that operate on the generic borrowed visual frame view from
//! `bmux_plugin` and emit compact, plugin-owned payloads.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::any::Any;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Once};

use bmux_plugin::{
    AttachVisualAdapter, AttachVisualAdapterOutput, AttachVisualAdapterRequest,
    AttachVisualSurfaceView, register_visual_adapter,
};
use serde::Serialize;

const PRESENCE_BITSET_ADAPTER_ID: &str = "bmux.visual.presence-bitset";

#[derive(Debug, Serialize)]
struct PresenceBitsetPayload {
    request_id: String,
    adapter: String,
    surface_id: uuid::Uuid,
    pane_id: uuid::Uuid,
    grid_revision: u64,
    encoding: String,
    width: u16,
    height: u16,
    words_per_row: u16,
    words: Vec<u32>,
}

#[derive(Default)]
struct PresenceBitsetCache {
    width: u16,
    height: u16,
    words_per_row: u16,
    row_hashes: Vec<Option<u64>>,
    row_words: Vec<u32>,
    #[cfg(test)]
    last_recomputed_rows: usize,
}

struct PresenceBitsetAdapter;

fn project_presence_bitset(
    surface: &dyn AttachVisualSurfaceView,
    request: &AttachVisualAdapterRequest,
    cache: &mut PresenceBitsetCache,
    out: &mut Vec<u8>,
) -> Result<AttachVisualAdapterOutput, String> {
    let width = surface.width();
    let height = surface.height();
    let words_per_row = width.saturating_add(31) / 32;
    let word_count = usize::from(words_per_row).saturating_mul(usize::from(height));
    if cache.width != width
        || cache.height != height
        || cache.words_per_row != words_per_row
        || cache.row_words.len() != word_count
    {
        cache.width = width;
        cache.height = height;
        cache.words_per_row = words_per_row;
        cache.row_hashes = vec![None; usize::from(height)];
        cache.row_words = vec![0; word_count];
    }

    #[cfg(test)]
    {
        cache.last_recomputed_rows = 0;
    }

    let mut row_words = vec![0_u32; usize::from(words_per_row)];
    for y in 0..height {
        row_words.fill(0);
        fill_presence_row_words(surface, width, words_per_row, y, &mut row_words);
        let row_hash = hash_presence_row(&row_words);
        let row_index = usize::from(y);
        if cache.row_hashes.get(row_index).copied().flatten() != Some(row_hash) {
            if let Some(hash) = cache.row_hashes.get_mut(row_index) {
                *hash = Some(row_hash);
            }
            let word_start = row_index.saturating_mul(usize::from(words_per_row));
            let word_end = word_start.saturating_add(usize::from(words_per_row));
            if let Some(words) = cache.row_words.get_mut(word_start..word_end) {
                words.copy_from_slice(&row_words);
            }
            #[cfg(test)]
            {
                cache.last_recomputed_rows = cache.last_recomputed_rows.saturating_add(1);
            }
        }
    }

    let payload = PresenceBitsetPayload {
        request_id: request.id.clone(),
        adapter: request.adapter.clone(),
        surface_id: surface.surface_id(),
        pane_id: surface.pane_id(),
        grid_revision: surface.grid_revision(),
        encoding: "u32-row-bitset-v1".to_string(),
        width,
        height,
        words_per_row,
        words: cache.row_words.clone(),
    };
    serde_json::to_writer(&mut *out, &payload).map_err(|error| error.to_string())?;
    Ok(AttachVisualAdapterOutput {
        encoding: "json".to_string(),
        payload: std::mem::take(out),
    })
}

fn fill_presence_row_words(
    surface: &dyn AttachVisualSurfaceView,
    width: u16,
    words_per_row: u16,
    y: u16,
    row_words: &mut [u32],
) {
    if words_per_row == 0 {
        return;
    }
    for x in 0..width {
        let occupied = surface
            .cell(x, y)
            .is_some_and(|cell| !cell.wide_continuation && !cell.text.trim().is_empty());
        if occupied {
            let word_index = usize::from(x / 32);
            let bit = u32::from(x % 32);
            if let Some(word) = row_words.get_mut(word_index) {
                *word |= 1_u32 << bit;
            }
        }
    }
}

fn hash_presence_row(row_words: &[u32]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    row_words.hash(&mut hasher);
    hasher.finish()
}

impl AttachVisualAdapter for PresenceBitsetAdapter {
    fn id(&self) -> &str {
        PRESENCE_BITSET_ADAPTER_ID
    }

    fn new_cache(&self, _request: &AttachVisualAdapterRequest) -> Option<Box<dyn Any + Send>> {
        Some(Box::<PresenceBitsetCache>::default())
    }

    fn project(
        &self,
        surface: &dyn AttachVisualSurfaceView,
        request: &AttachVisualAdapterRequest,
        out: &mut Vec<u8>,
    ) -> Result<AttachVisualAdapterOutput, String> {
        let mut cache = PresenceBitsetCache::default();
        project_presence_bitset(surface, request, &mut cache, out)
    }

    fn project_cached(
        &self,
        surface: &dyn AttachVisualSurfaceView,
        request: &AttachVisualAdapterRequest,
        cache: Option<&mut dyn Any>,
        out: &mut Vec<u8>,
    ) -> Result<AttachVisualAdapterOutput, String> {
        let Some(cache) = cache.and_then(|cache| cache.downcast_mut::<PresenceBitsetCache>())
        else {
            return self.project(surface, request, out);
        };
        project_presence_bitset(surface, request, cache, out)
    }
}

/// Register bundled visual adapters. Idempotent.
pub fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        register_visual_adapter(Arc::new(PresenceBitsetAdapter));
        tracing::debug!(
            adapter = PRESENCE_BITSET_ADAPTER_ID,
            "registered visual adapter"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{AttachVisualCellRef, ExtensionRect};
    use std::collections::BTreeMap;
    use uuid::Uuid;

    struct TestSurface {
        surface_id: Uuid,
        pane_id: Uuid,
        revision: u64,
        rows: Vec<Vec<String>>,
    }

    impl TestSurface {
        fn new(rows: &[&[&str]], revision: u64) -> Self {
            Self {
                surface_id: Uuid::from_u128(1),
                pane_id: Uuid::from_u128(2),
                revision,
                rows: rows
                    .iter()
                    .map(|row| row.iter().map(|cell| (*cell).to_string()).collect())
                    .collect(),
            }
        }
    }

    impl AttachVisualSurfaceView for TestSurface {
        fn surface_id(&self) -> Uuid {
            self.surface_id
        }

        fn pane_id(&self) -> Uuid {
            self.pane_id
        }

        fn rect(&self) -> ExtensionRect {
            ExtensionRect::new(0, 0, self.width(), self.height())
        }

        fn content_rect(&self) -> ExtensionRect {
            ExtensionRect::new(0, 0, self.width(), self.height())
        }

        fn focused(&self) -> bool {
            true
        }

        fn grid_revision(&self) -> u64 {
            self.revision
        }

        fn width(&self) -> u16 {
            self.rows
                .first()
                .map_or(0, |row| u16::try_from(row.len()).unwrap_or(u16::MAX))
        }

        fn height(&self) -> u16 {
            u16::try_from(self.rows.len()).unwrap_or(u16::MAX)
        }

        fn cell(&self, x: u16, y: u16) -> Option<AttachVisualCellRef<'_>> {
            let text = self.rows.get(usize::from(y))?.get(usize::from(x))?;
            Some(AttachVisualCellRef {
                text,
                width: 1,
                wide_continuation: false,
            })
        }
    }

    fn request() -> AttachVisualAdapterRequest {
        AttachVisualAdapterRequest {
            id: "test.presence".to_string(),
            adapter: PRESENCE_BITSET_ADAPTER_ID.to_string(),
            owner_plugin_id: "test".to_string(),
            event_kind: "test".to_string(),
            scope: "focused-pane".to_string(),
            area: "content".to_string(),
            max_hz: 0,
            dirty_only: true,
            max_bytes: 4096,
            settings: BTreeMap::new(),
        }
    }

    #[test]
    fn presence_bitset_cache_reuses_unchanged_rows() {
        let adapter = PresenceBitsetAdapter;
        let request = request();
        let mut cache = PresenceBitsetCache::default();
        let surface = TestSurface::new(&[&["x", " ", "x"], &[" ", "y", " "]], 1);
        let mut out = Vec::new();

        let first = adapter
            .project_cached(&surface, &request, Some(&mut cache), &mut out)
            .expect("first projection succeeds");
        assert_eq!(cache.last_recomputed_rows, 2);
        let first_payload = first.payload;

        let second = adapter
            .project_cached(&surface, &request, Some(&mut cache), &mut out)
            .expect("second projection succeeds");
        assert_eq!(cache.last_recomputed_rows, 0);
        assert_eq!(second.payload, first_payload);

        let changed = TestSurface::new(&[&["x", " ", "x"], &["z", "y", " "]], 2);
        let changed_output = adapter
            .project_cached(&changed, &request, Some(&mut cache), &mut out)
            .expect("changed projection succeeds");
        assert_eq!(cache.last_recomputed_rows, 1);
        assert_ne!(changed_output.payload, first_payload);
    }

    #[test]
    fn presence_bitset_cached_payload_matches_full_projection() {
        let adapter = PresenceBitsetAdapter;
        let request = request();
        let surface = TestSurface::new(&[&["x", " ", "x"], &[" ", "y", " "]], 1);
        let mut cached_cache = PresenceBitsetCache::default();
        let mut cached = Vec::new();
        let mut full = Vec::new();

        let cached = adapter
            .project_cached(&surface, &request, Some(&mut cached_cache), &mut cached)
            .expect("cached projection succeeds")
            .payload;
        let full = adapter
            .project(&surface, &request, &mut full)
            .expect("full projection succeeds")
            .payload;

        assert_eq!(cached, full);
    }
}
