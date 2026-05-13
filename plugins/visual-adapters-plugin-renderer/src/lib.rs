//! Bundled attach-local visual adapters.
//!
//! This crate intentionally lives outside core. It registers common adapter
//! implementations that operate on the generic borrowed visual frame view from
//! `bmux_plugin` and emit compact, plugin-owned payloads.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::any::Any;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Once};

use bmux_plugin::{
    AttachVisualAdapter, AttachVisualAdapterOutput, AttachVisualAdapterRequest,
    AttachVisualProjectionResult, AttachVisualSurfaceView, register_visual_adapter,
};

const PRESENCE_BITSET_ADAPTER_ID: &str = "bmux.visual.presence-bitset";
const U32_JSON_SLOT_WIDTH: usize = 10;
const U64_JSON_SLOT_WIDTH: usize = 20;
const PRESENCE_ENCODING: &str = "u32-row-bitset-v1";
const PRESENCE_STATS_LOG_EVERY: u64 = 256;

#[derive(Default)]
struct PresenceBitsetCache {
    width: u16,
    height: u16,
    words_per_row: u16,
    row_fingerprints: Vec<Option<u64>>,
    row_hashes: Vec<Option<u64>>,
    row_words: Vec<u32>,
    payload: Vec<u8>,
    grid_revision_offset: Option<usize>,
    word_offsets: Vec<usize>,
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
    let resized = ensure_presence_cache(surface, request, cache, width, height, words_per_row)?;
    cache.projections = cache.projections.saturating_add(1);

    #[cfg(test)]
    {
        cache.last_recomputed_rows = 0;
    }

    let mut changed = resized;
    let mut row_words = vec![0_u32; usize::from(words_per_row)];
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
        row_words.fill(0);
        fill_presence_row_words(surface, width, words_per_row, y, &mut row_words);
        let row_hash = hash_presence_row(&row_words);
        if let Some(fingerprint) = cache.row_fingerprints.get_mut(row_index) {
            *fingerprint = row_fingerprint;
        }
        if cache.row_hashes.get(row_index).copied().flatten() != Some(row_hash) {
            if let Some(hash) = cache.row_hashes.get_mut(row_index) {
                *hash = Some(row_hash);
            }
            let word_start = row_index.saturating_mul(usize::from(words_per_row));
            let word_end = word_start.saturating_add(usize::from(words_per_row));
            if let Some(words) = cache.row_words.get_mut(word_start..word_end) {
                words.copy_from_slice(&row_words);
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
            encoding: "json".to_string(),
            payload: std::mem::take(out),
        },
    ))
}

fn ensure_presence_cache(
    surface: &dyn AttachVisualSurfaceView,
    request: &AttachVisualAdapterRequest,
    cache: &mut PresenceBitsetCache,
    width: u16,
    height: u16,
    words_per_row: u16,
) -> Result<bool, String> {
    let word_count = usize::from(words_per_row).saturating_mul(usize::from(height));
    if cache.width == width
        && cache.height == height
        && cache.words_per_row == words_per_row
        && cache.row_words.len() == word_count
        && !cache.payload.is_empty()
    {
        return Ok(false);
    }

    cache.width = width;
    cache.height = height;
    cache.words_per_row = words_per_row;
    cache.row_fingerprints = vec![None; usize::from(height)];
    cache.row_hashes = vec![None; usize::from(height)];
    cache.row_words = vec![0; word_count];
    cache.cache_rebuilds = cache.cache_rebuilds.saturating_add(1);
    rebuild_presence_payload(surface, request, cache)?;
    Ok(true)
}

fn rebuild_presence_payload(
    surface: &dyn AttachVisualSurfaceView,
    request: &AttachVisualAdapterRequest,
    cache: &mut PresenceBitsetCache,
) -> Result<(), String> {
    let mut payload = String::new();
    cache.word_offsets.clear();
    cache.grid_revision_offset = None;

    payload.push('{');
    push_json_string_field(&mut payload, "request_id", &request.id)?;
    payload.push(',');
    push_json_string_field(&mut payload, "adapter", &request.adapter)?;
    payload.push(',');
    push_json_string_field(
        &mut payload,
        "surface_id",
        &surface.surface_id().to_string(),
    )?;
    payload.push(',');
    push_json_string_field(&mut payload, "pane_id", &surface.pane_id().to_string())?;
    payload.push_str(",\"grid_revision\":");
    cache.grid_revision_offset = Some(payload.len());
    push_fixed_u64(&mut payload, surface.grid_revision())?;
    payload.push(',');
    push_json_string_field(&mut payload, "encoding", PRESENCE_ENCODING)?;
    write!(
        payload,
        ",\"width\":{},\"height\":{},\"words_per_row\":{},\"words\":[",
        cache.width, cache.height, cache.words_per_row
    )
    .map_err(|error| error.to_string())?;
    for index in 0..cache.row_words.len() {
        if index > 0 {
            payload.push(',');
        }
        cache.word_offsets.push(payload.len());
        push_fixed_u32(&mut payload, cache.row_words[index])?;
    }
    payload.push_str("]}");
    cache.payload = payload.into_bytes();
    Ok(())
}

fn push_json_string_field(payload: &mut String, name: &str, value: &str) -> Result<(), String> {
    let value = serde_json::to_string(value).map_err(|error| error.to_string())?;
    write!(payload, "\"{name}\":{value}").map_err(|error| error.to_string())
}

fn push_fixed_u32(payload: &mut String, value: u32) -> Result<(), String> {
    write!(payload, "{value:>U32_JSON_SLOT_WIDTH$}").map_err(|error| error.to_string())
}

fn push_fixed_u64(payload: &mut String, value: u64) -> Result<(), String> {
    write!(payload, "{value:>U64_JSON_SLOT_WIDTH$}").map_err(|error| error.to_string())
}

fn patch_presence_payload_grid_revision(
    cache: &mut PresenceBitsetCache,
    revision: u64,
) -> Result<(), String> {
    let Some(offset) = cache.grid_revision_offset else {
        return Err("presence-bitset payload grid revision offset is missing".to_string());
    };
    patch_fixed_number(&mut cache.payload, offset, U64_JSON_SLOT_WIDTH, revision)
}

fn patch_presence_payload_row(
    cache: &mut PresenceBitsetCache,
    row_index: usize,
) -> Result<(), String> {
    let words_per_row = usize::from(cache.words_per_row);
    let word_start = row_index.saturating_mul(words_per_row);
    let word_end = word_start.saturating_add(words_per_row);
    for word_index in word_start..word_end {
        let Some(offset) = cache.word_offsets.get(word_index).copied() else {
            return Err("presence-bitset payload word offset is missing".to_string());
        };
        let Some(word) = cache.row_words.get(word_index).copied() else {
            return Err("presence-bitset row word is missing".to_string());
        };
        patch_fixed_number(&mut cache.payload, offset, U32_JSON_SLOT_WIDTH, word)?;
    }
    Ok(())
}

fn patch_fixed_number<T>(
    payload: &mut [u8],
    offset: usize,
    width: usize,
    value: T,
) -> Result<(), String>
where
    T: std::fmt::Display,
{
    let encoded = format!("{value:>width$}");
    if encoded.len() != width {
        return Err("presence-bitset payload slot width is too small".to_string());
    }
    let end = offset.saturating_add(width);
    let Some(slot) = payload.get_mut(offset..end) else {
        return Err("presence-bitset payload slot is out of bounds".to_string());
    };
    slot.copy_from_slice(encoded.as_bytes());
    Ok(())
}

fn cached_presence_output(
    cache: &PresenceBitsetCache,
) -> Result<AttachVisualAdapterOutput, String> {
    if cache.payload.is_empty() {
        return Err("presence-bitset cached payload is empty".to_string());
    }
    Ok(AttachVisualAdapterOutput {
        encoding: "json".to_string(),
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
            .project_incremental_cached(&surface, &request, Some(&mut cache), &mut out)
            .expect("second projection succeeds");
        assert_eq!(cache.last_recomputed_rows, 0);
        assert_eq!(cache.rows_reused_by_fingerprint, 2);
        assert_eq!(second, AttachVisualProjectionResult::Unchanged);

        let changed = TestSurface::new(&[&["x", " ", "x"], &["z", "y", " "]], 2);
        let changed_output = adapter
            .project_cached(&changed, &request, Some(&mut cache), &mut out)
            .expect("changed projection succeeds");
        assert_eq!(cache.last_recomputed_rows, 1);
        assert_eq!(cache.projections, 3);
        assert_eq!(cache.updated, 2);
        assert_eq!(cache.unchanged, 1);
        assert_eq!(cache.rows_reused_by_fingerprint, 3);
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

    #[test]
    fn presence_bitset_semantic_noop_returns_unchanged() {
        let adapter = PresenceBitsetAdapter;
        let request = request();
        let mut cache = PresenceBitsetCache::default();
        let mut out = Vec::new();
        let initial = TestSurface::new(&[&["x", " ", "x"]], 1);
        let renamed = TestSurface::new(&[&["z", " ", "q"]], 2);

        assert!(matches!(
            adapter
                .project_incremental_cached(&initial, &request, Some(&mut cache), &mut out)
                .expect("initial projection succeeds"),
            AttachVisualProjectionResult::Updated(_)
        ));
        let noop = adapter
            .project_incremental_cached(&renamed, &request, Some(&mut cache), &mut out)
            .expect("semantic noop projection succeeds");

        assert_eq!(noop, AttachVisualProjectionResult::Unchanged);
    }

    #[test]
    fn presence_bitset_resize_invalidates_cache() {
        let adapter = PresenceBitsetAdapter;
        let request = request();
        let mut cache = PresenceBitsetCache::default();
        let mut out = Vec::new();
        let initial = TestSurface::new(&[&["x", " ", "x"]], 1);
        let resized = TestSurface::new(&[&["x", " ", "x", " "]], 2);

        adapter
            .project_incremental_cached(&initial, &request, Some(&mut cache), &mut out)
            .expect("initial projection succeeds");
        let resized = adapter
            .project_incremental_cached(&resized, &request, Some(&mut cache), &mut out)
            .expect("resize projection succeeds");

        assert!(matches!(resized, AttachVisualProjectionResult::Updated(_)));
        assert_eq!(cache.width, 4);
    }

    #[test]
    fn presence_bitset_payload_remains_json_compatible() {
        let adapter = PresenceBitsetAdapter;
        let request = request();
        let surface = TestSurface::new(&[&["x", " ", "x"], &[" ", "y", " "]], 1);
        let mut out = Vec::new();

        let payload = adapter
            .project(&surface, &request, &mut out)
            .expect("projection succeeds")
            .payload;
        let decoded: serde_json::Value = serde_json::from_slice(&payload).expect("valid json");

        assert_eq!(decoded["encoding"], PRESENCE_ENCODING);
        assert_eq!(decoded["words"].as_array().expect("words array").len(), 2);
    }
}
