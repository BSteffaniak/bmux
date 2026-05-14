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
    AttachVisualProjectionResult, AttachVisualSurfaceView, register_visual_adapter,
};

const PRESENCE_BITSET_ADAPTER_ID: &str = "bmux.visual.presence-bitset";
const PRESENCE_ENCODING: &str = "presence-bitset-bin-v1";
const PRESENCE_MAGIC: &[u8; 4] = b"PBB1";
const PRESENCE_HEADER_LEN: usize = 20;
const PRESENCE_GRID_REVISION_OFFSET: usize = 12;
const PRESENCE_WORDS_OFFSET: usize = PRESENCE_HEADER_LEN;
const PRESENCE_STATS_LOG_EVERY: u64 = 256;

#[derive(Default)]
struct PresenceBitsetCache {
    width: u16,
    height: u16,
    words_per_row: u16,
    row_fingerprints: Vec<Option<u64>>,
    row_hashes: Vec<Option<u64>>,
    scratch_row_words: Vec<u32>,
    payload: Vec<u8>,
    projections: u64,
    unchanged: u64,
    updated: u64,
    cache_rebuilds: u64,
    emitted_bytes: u64,
    rows_scanned: u64,
    rows_reused_by_fingerprint: u64,
    rows_changed: u64,
    #[cfg(test)]
    last_recomputed_rows: usize,
}

struct PresenceBitsetAdapter;

fn project_presence_bitset(
    surface: &dyn AttachVisualSurfaceView,
    request: &AttachVisualAdapterRequest,
    cache: &mut PresenceBitsetCache,
    out: &mut Vec<u8>,
) -> Result<AttachVisualProjectionResult, String> {
    let width = surface.width();
    let height = surface.height();
    let words_per_row = width.saturating_add(31) / 32;
    let resized = ensure_presence_cache(cache, width, height, words_per_row)?;
    cache.projections = cache.projections.saturating_add(1);

    #[cfg(test)]
    {
        cache.last_recomputed_rows = 0;
    }

    let mut changed = resized;
    for y in 0..height {
        let row_index = usize::from(y);
        let row_fingerprint = surface.row_content_fingerprint(y);
        if !resized
            && row_fingerprint.is_some()
            && cache.row_fingerprints.get(row_index).copied().flatten() == row_fingerprint
        {
            cache.rows_reused_by_fingerprint = cache.rows_reused_by_fingerprint.saturating_add(1);
            continue;
        }

        cache.rows_scanned = cache.rows_scanned.saturating_add(1);
        cache.scratch_row_words.fill(0);
        fill_presence_row_words(
            surface,
            width,
            words_per_row,
            y,
            &mut cache.scratch_row_words,
        );
        let row_hash = hash_presence_row(&cache.scratch_row_words);
        if let Some(fingerprint) = cache.row_fingerprints.get_mut(row_index) {
            *fingerprint = row_fingerprint;
        }
        if cache.row_hashes.get(row_index).copied().flatten() != Some(row_hash) {
            if let Some(hash) = cache.row_hashes.get_mut(row_index) {
                *hash = Some(row_hash);
            }
            patch_presence_payload_row(cache, row_index)?;
            cache.rows_changed = cache.rows_changed.saturating_add(1);
            changed = true;
            #[cfg(test)]
            {
                cache.last_recomputed_rows = cache.last_recomputed_rows.saturating_add(1);
            }
        }
    }

    if !changed {
        cache.unchanged = cache.unchanged.saturating_add(1);
        maybe_log_presence_stats(surface, request, cache);
        return Ok(AttachVisualProjectionResult::Unchanged);
    }

    patch_presence_payload_grid_revision(cache, surface.grid_revision())?;
    cache.updated = cache.updated.saturating_add(1);
    cache.emitted_bytes = cache
        .emitted_bytes
        .saturating_add(u64::try_from(cache.payload.len()).unwrap_or(u64::MAX));
    maybe_log_presence_stats(surface, request, cache);
    out.clear();
    out.extend_from_slice(&cache.payload);
    Ok(AttachVisualProjectionResult::Updated(
        AttachVisualAdapterOutput {
            encoding: PRESENCE_ENCODING.to_string(),
            payload: std::mem::take(out),
        },
    ))
}

fn ensure_presence_cache(
    cache: &mut PresenceBitsetCache,
    width: u16,
    height: u16,
    words_per_row: u16,
) -> Result<bool, String> {
    let word_count = usize::from(words_per_row).saturating_mul(usize::from(height));
    let expected_payload_len = PRESENCE_HEADER_LEN.saturating_add(word_count.saturating_mul(4));
    if cache.width == width
        && cache.height == height
        && cache.words_per_row == words_per_row
        && cache.scratch_row_words.len() == usize::from(words_per_row)
        && cache.payload.len() == expected_payload_len
    {
        return Ok(false);
    }

    cache.width = width;
    cache.height = height;
    cache.words_per_row = words_per_row;
    cache.row_fingerprints = vec![None; usize::from(height)];
    cache.row_hashes = vec![None; usize::from(height)];
    cache.scratch_row_words = vec![0; usize::from(words_per_row)];
    cache.cache_rebuilds = cache.cache_rebuilds.saturating_add(1);
    rebuild_presence_payload(cache)?;
    Ok(true)
}

fn rebuild_presence_payload(cache: &mut PresenceBitsetCache) -> Result<(), String> {
    let word_bytes = usize::from(cache.words_per_row)
        .saturating_mul(usize::from(cache.height))
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| "presence-bitset payload is too large".to_string())?;
    let payload_len = PRESENCE_HEADER_LEN
        .checked_add(word_bytes)
        .ok_or_else(|| "presence-bitset payload is too large".to_string())?;
    cache.payload = vec![0; payload_len];
    cache.payload[0..4].copy_from_slice(PRESENCE_MAGIC);
    cache.payload[4..6].copy_from_slice(&cache.width.to_le_bytes());
    cache.payload[6..8].copy_from_slice(&cache.height.to_le_bytes());
    cache.payload[8..10].copy_from_slice(&cache.words_per_row.to_le_bytes());
    cache.payload[10..12].copy_from_slice(&0_u16.to_le_bytes());
    patch_presence_payload_grid_revision(cache, 0)?;
    Ok(())
}

fn patch_presence_payload_grid_revision(
    cache: &mut PresenceBitsetCache,
    revision: u64,
) -> Result<(), String> {
    let end = PRESENCE_GRID_REVISION_OFFSET.saturating_add(std::mem::size_of::<u64>());
    let Some(slot) = cache.payload.get_mut(PRESENCE_GRID_REVISION_OFFSET..end) else {
        return Err("presence-bitset payload grid revision slot is missing".to_string());
    };
    slot.copy_from_slice(&revision.to_le_bytes());
    Ok(())
}

fn patch_presence_payload_row(
    cache: &mut PresenceBitsetCache,
    row_index: usize,
) -> Result<(), String> {
    let words_per_row = usize::from(cache.words_per_row);
    if cache.scratch_row_words.len() != words_per_row {
        return Err("presence-bitset row scratch size mismatch".to_string());
    }
    let word_start = row_index.saturating_mul(words_per_row);
    for (local_word_index, word) in cache.scratch_row_words.iter().copied().enumerate() {
        let word_index = word_start.saturating_add(local_word_index);
        let offset = PRESENCE_WORDS_OFFSET.saturating_add(word_index.saturating_mul(4));
        let end = offset.saturating_add(4);
        let Some(slot) = cache.payload.get_mut(offset..end) else {
            return Err("presence-bitset payload word slot is missing".to_string());
        };
        slot.copy_from_slice(&word.to_le_bytes());
    }
    Ok(())
}

fn cached_presence_output(
    cache: &PresenceBitsetCache,
) -> Result<AttachVisualAdapterOutput, String> {
    if cache.payload.is_empty() {
        return Err("presence-bitset cached payload is empty".to_string());
    }
    Ok(AttachVisualAdapterOutput {
        encoding: PRESENCE_ENCODING.to_string(),
        payload: cache.payload.clone(),
    })
}

fn maybe_log_presence_stats(
    surface: &dyn AttachVisualSurfaceView,
    request: &AttachVisualAdapterRequest,
    cache: &PresenceBitsetCache,
) {
    if !cache.projections.is_multiple_of(PRESENCE_STATS_LOG_EVERY) {
        return;
    }
    tracing::debug!(
        request_id = %request.id,
        adapter = %request.adapter,
        surface_id = %surface.surface_id(),
        pane_id = %surface.pane_id(),
        projections = cache.projections,
        unchanged = cache.unchanged,
        updated = cache.updated,
        cache_rebuilds = cache.cache_rebuilds,
        emitted_bytes = cache.emitted_bytes,
        rows_scanned = cache.rows_scanned,
        rows_reused_by_fingerprint = cache.rows_reused_by_fingerprint,
        rows_changed = cache.rows_changed,
        width = cache.width,
        height = cache.height,
        words_per_row = cache.words_per_row,
        "presence-bitset visual adapter stats",
    );
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
        match project_presence_bitset(surface, request, &mut cache, out)? {
            AttachVisualProjectionResult::Updated(output) => Ok(output),
            AttachVisualProjectionResult::Unchanged => {
                patch_presence_payload_grid_revision(&mut cache, surface.grid_revision())?;
                cached_presence_output(&cache)
            }
        }
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
        match project_presence_bitset(surface, request, cache, out)? {
            AttachVisualProjectionResult::Updated(output) => Ok(output),
            AttachVisualProjectionResult::Unchanged => {
                patch_presence_payload_grid_revision(cache, surface.grid_revision())?;
                cached_presence_output(cache)
            }
        }
    }

    fn project_incremental_cached(
        &self,
        surface: &dyn AttachVisualSurfaceView,
        request: &AttachVisualAdapterRequest,
        cache: Option<&mut dyn Any>,
        out: &mut Vec<u8>,
    ) -> Result<AttachVisualProjectionResult, String> {
        let Some(cache) = cache.and_then(|cache| cache.downcast_mut::<PresenceBitsetCache>())
        else {
            return self
                .project(surface, request, out)
                .map(AttachVisualProjectionResult::Updated);
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

    struct DecodedPresence<'a> {
        width: u16,
        height: u16,
        words_per_row: u16,
        grid_revision: u64,
        words: Vec<u32>,
        payload: &'a [u8],
    }

    fn decode_presence(payload: &[u8]) -> DecodedPresence<'_> {
        assert!(payload.len() >= PRESENCE_HEADER_LEN);
        assert_eq!(&payload[0..4], PRESENCE_MAGIC);
        let width = u16::from_le_bytes(payload[4..6].try_into().expect("width"));
        let height = u16::from_le_bytes(payload[6..8].try_into().expect("height"));
        let words_per_row = u16::from_le_bytes(payload[8..10].try_into().expect("words_per_row"));
        let grid_revision = u64::from_le_bytes(payload[12..20].try_into().expect("revision"));
        let words = payload[PRESENCE_WORDS_OFFSET..]
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("word")))
            .collect();
        DecodedPresence {
            width,
            height,
            words_per_row,
            grid_revision,
            words,
            payload,
        }
    }

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

        fn row_content_fingerprint(&self, row: u16) -> Option<u64> {
            let row = self.rows.get(usize::from(row))?;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            row.hash(&mut hasher);
            Some(hasher.finish())
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

    fn project(surface: &TestSurface) -> AttachVisualAdapterOutput {
        PresenceBitsetAdapter
            .project(surface, &request(), &mut Vec::new())
            .expect("project")
    }

    #[test]
    fn presence_bitset_projects_binary_header_and_words() {
        let surface = TestSurface::new(&[&["x", " ", "y"], &[" ", "z", " "]], 42);
        let output = project(&surface);
        assert_eq!(output.encoding, PRESENCE_ENCODING);
        let decoded = decode_presence(&output.payload);
        assert_eq!(decoded.width, 3);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.words_per_row, 1);
        assert_eq!(decoded.grid_revision, 42);
        assert_eq!(decoded.words, vec![0b101, 0b010]);
        assert_eq!(decoded.payload.len(), PRESENCE_HEADER_LEN + 8);
    }

    #[test]
    fn presence_bitset_cache_reuses_fingerprinted_rows() {
        let mut cache = PresenceBitsetCache::default();
        let mut out = Vec::new();
        let surface = TestSurface::new(&[&["x", " ", "y"], &[" ", "z", " "]], 1);
        let first =
            project_presence_bitset(&surface, &request(), &mut cache, &mut out).expect("first");
        assert!(matches!(first, AttachVisualProjectionResult::Updated(_)));
        assert_eq!(cache.last_recomputed_rows, 2);

        let surface = TestSurface::new(&[&["x", " ", "y"], &[" ", "z", " "]], 2);
        let second =
            project_presence_bitset(&surface, &request(), &mut cache, &mut out).expect("second");
        assert_eq!(second, AttachVisualProjectionResult::Unchanged);
        assert_eq!(cache.last_recomputed_rows, 0);
        assert_eq!(cache.unchanged, 1);
    }

    #[test]
    fn presence_bitset_cached_projection_matches_uncached_projection() {
        let surface = TestSurface::new(&[&["x", " ", "y"], &[" ", "z", " "]], 7);
        let uncached = project(&surface);
        let adapter = PresenceBitsetAdapter;
        let mut cache = adapter.new_cache(&request()).expect("cache");
        let cached = adapter
            .project_cached(&surface, &request(), Some(cache.as_mut()), &mut Vec::new())
            .expect("cached");
        assert_eq!(cached.encoding, uncached.encoding);
        assert_eq!(cached.payload, uncached.payload);
    }

    #[test]
    fn presence_bitset_semantic_noop_returns_unchanged() {
        let mut cache = PresenceBitsetCache::default();
        let mut out = Vec::new();
        let first_surface = TestSurface::new(&[&["x", " ", "y"]], 1);
        let first = project_presence_bitset(&first_surface, &request(), &mut cache, &mut out)
            .expect("first");
        assert!(matches!(first, AttachVisualProjectionResult::Updated(_)));

        let second_surface = TestSurface::new(&[&["a", " ", "b"]], 2);
        let second = project_presence_bitset(&second_surface, &request(), &mut cache, &mut out)
            .expect("second");
        assert_eq!(second, AttachVisualProjectionResult::Unchanged);
        assert_eq!(cache.last_recomputed_rows, 0);
    }

    #[test]
    fn presence_bitset_resize_rebuilds_payload() {
        let mut cache = PresenceBitsetCache::default();
        let mut out = Vec::new();
        let first_surface = TestSurface::new(&[&["x", " "]], 1);
        let first = project_presence_bitset(&first_surface, &request(), &mut cache, &mut out)
            .expect("first");
        assert!(matches!(first, AttachVisualProjectionResult::Updated(_)));

        let second_surface = TestSurface::new(&[&["x", " ", "y"], &[" ", "z", " "]], 2);
        let second = project_presence_bitset(&second_surface, &request(), &mut cache, &mut out)
            .expect("second");
        assert!(matches!(second, AttachVisualProjectionResult::Updated(_)));
        let decoded = decode_presence(&cache.payload);
        assert_eq!(decoded.width, 3);
        assert_eq!(decoded.height, 2);
        assert_eq!(decoded.words, vec![0b101, 0b010]);
    }

    #[test]
    fn presence_bitset_cached_payload_patches_changed_rows() {
        let mut cache = PresenceBitsetCache::default();
        let mut out = Vec::new();
        let first_surface = TestSurface::new(&[&["x", " ", "y"], &[" ", "z", " "]], 1);
        let first = project_presence_bitset(&first_surface, &request(), &mut cache, &mut out)
            .expect("first");
        assert!(matches!(first, AttachVisualProjectionResult::Updated(_)));

        let second_surface = TestSurface::new(&[&["x", " ", "y"], &["q", "z", " "]], 2);
        let second = project_presence_bitset(&second_surface, &request(), &mut cache, &mut out)
            .expect("second");
        assert!(matches!(second, AttachVisualProjectionResult::Updated(_)));
        assert_eq!(cache.last_recomputed_rows, 1);
        let decoded = decode_presence(&cache.payload);
        assert_eq!(decoded.grid_revision, 2);
        assert_eq!(decoded.words, vec![0b101, 0b011]);
    }
}
