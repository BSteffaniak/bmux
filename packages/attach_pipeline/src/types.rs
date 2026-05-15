use bmux_attach_layout_protocol::{AttachInputModeState, AttachMouseProtocolState};
use bmux_plugin::{ExtensionRect, RenderDamage, TerminalGraphicFill, TerminalRgba};
use bmux_terminal_grid::{GridLimits, PhysicalRow, TerminalGridStream, TerminalProtocolTracker};
use std::collections::BTreeMap;
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttachCursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachScrollbackCursor {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttachScrollbackPosition {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRenderCacheEntry {
    pub surface_id: Uuid,
    pub surface_rect: ExtensionRect,
    pub damage: RenderDamage,
    pub revision: u64,
    pub bytes: Vec<u8>,
}

pub struct PaneRenderBuffer {
    pub terminal_grid: TerminalGridStream,
    pub protocol_tracker: TerminalProtocolTracker,
    pub prev_rows: Vec<String>,
    pub sync_update_in_progress: bool,
    pub expected_stream_start: Option<u64>,
    pub scrollback_window: Option<PaneScrollbackWindow>,
    pub extension_render_cache: BTreeMap<(String, Uuid), ExtensionRenderCacheEntry>,
    pub terminal_graphics_cache: BTreeMap<u64, TerminalGraphicCacheEntry>,
    pub visual_row_fingerprints: PaneVisualRowFingerprintCache,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalGraphicCacheEntry {
    pub surface_id: Uuid,
    pub signature: TerminalGraphicSignature,
    pub host_image_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TerminalGraphicSignature {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub color: TerminalRgba,
    pub fill: TerminalGraphicFill,
}

#[derive(Default)]
pub struct PaneVisualRowFingerprintCache {
    inner: Mutex<PaneVisualRowFingerprintState>,
}

#[derive(Default)]
struct PaneVisualRowFingerprintState {
    width: u16,
    height: u16,
    content_revision: Option<u64>,
    rows: Vec<Option<u64>>,
}

impl PaneVisualRowFingerprintCache {
    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = PaneVisualRowFingerprintState::default();
        }
    }

    pub fn invalidate_rows(&self, reset_rows: bool, content_revision: u64, row_updates: &[u32]) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if reset_rows {
            *guard = PaneVisualRowFingerprintState {
                content_revision: Some(content_revision),
                ..PaneVisualRowFingerprintState::default()
            };
            return;
        }
        guard.content_revision = Some(content_revision);
        for row in row_updates {
            let index = usize::try_from(*row).unwrap_or(usize::MAX);
            if let Some(fingerprint) = guard.rows.get_mut(index) {
                *fingerprint = None;
            }
        }
    }

    pub fn get_or_compute(
        &self,
        width: u16,
        height: u16,
        content_revision: u64,
        row: u16,
        compute: impl FnOnce() -> Option<u64>,
    ) -> Option<u64> {
        if row >= height {
            return None;
        }
        if let Ok(mut guard) = self.inner.lock() {
            guard.ensure_dimensions(width, height, content_revision);
            if let Some(Some(fingerprint)) = guard.rows.get(usize::from(row)) {
                return Some(*fingerprint);
            }
        }

        let fingerprint = compute()?;
        if let Ok(mut guard) = self.inner.lock() {
            guard.ensure_dimensions(width, height, content_revision);
            if let Some(slot) = guard.rows.get_mut(usize::from(row)) {
                *slot = Some(fingerprint);
            }
        }
        Some(fingerprint)
    }
}

impl PaneVisualRowFingerprintState {
    fn ensure_dimensions(&mut self, width: u16, height: u16, content_revision: u64) {
        if self.width == width
            && self.height == height
            && self.content_revision == Some(content_revision)
            && self.rows.len() == usize::from(height)
        {
            return;
        }
        self.width = width;
        self.height = height;
        self.content_revision = Some(content_revision);
        self.rows = vec![None; usize::from(height)];
    }
}

pub struct PaneScrollbackWindow {
    pub scrollback_offset: usize,
    pub max_scrollback_offset: usize,
    pub total_scrolled_rows: u64,
    pub rows: Vec<PhysicalRow>,
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            terminal_grid: TerminalGridStream::new(80, 24, GridLimits::default())
                .expect("default pane render grid dimensions are valid"),
            protocol_tracker: TerminalProtocolTracker::new(),
            prev_rows: Vec::new(),
            sync_update_in_progress: false,
            expected_stream_start: None,
            scrollback_window: None,
            extension_render_cache: BTreeMap::new(),
            terminal_graphics_cache: BTreeMap::new(),
            visual_row_fingerprints: PaneVisualRowFingerprintCache::default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AttachPaneMouseProtocolHints {
    pub mode_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pub input_mode_hints: BTreeMap<Uuid, AttachInputModeState>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn visual_row_fingerprint_cache_reuses_rows_and_invalidates_precisely() {
        let cache = PaneVisualRowFingerprintCache::default();
        let calls = AtomicUsize::new(0);

        let first = cache.get_or_compute(80, 24, 1, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(10)
        });
        let cached = cache.get_or_compute(80, 24, 1, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(11)
        });

        assert_eq!(first, Some(10));
        assert_eq!(cached, Some(10));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cache.invalidate_rows(false, 2, &[4]);
        let still_cached = cache.get_or_compute(80, 24, 2, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(12)
        });
        assert_eq!(still_cached, Some(10));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cache.invalidate_rows(false, 3, &[3]);
        let recomputed = cache.get_or_compute(80, 24, 3, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(13)
        });
        assert_eq!(recomputed, Some(13));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn visual_row_fingerprint_cache_recomputes_on_untracked_revision_change() {
        let cache = PaneVisualRowFingerprintCache::default();
        let calls = AtomicUsize::new(0);

        assert_eq!(
            cache.get_or_compute(80, 24, 1, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(1)
            }),
            Some(1)
        );
        assert_eq!(
            cache.get_or_compute(80, 24, 2, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(2)
            }),
            Some(2)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn visual_row_fingerprint_cache_resets_on_dimension_change() {
        let cache = PaneVisualRowFingerprintCache::default();
        let calls = AtomicUsize::new(0);

        assert_eq!(
            cache.get_or_compute(80, 24, 1, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(1)
            }),
            Some(1)
        );
        assert_eq!(
            cache.get_or_compute(81, 24, 1, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(2)
            }),
            Some(2)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
