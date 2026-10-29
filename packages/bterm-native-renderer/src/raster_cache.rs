//! Small renderer-private LRU. Fingerprints accelerate lookup; equality decides identity.
use std::{collections::VecDeque, sync::Arc};

struct Entry<K, V> {
    fingerprint: u64,
    key: K,
    value: Arc<V>,
    bytes: usize,
}

pub(crate) struct Cache<K, V> {
    entries: VecDeque<Entry<K, V>>,
    bytes: usize,
    limit: usize,
    capacity: usize,
}

impl<K, V> Cache<K, V> {
    pub(crate) fn new(limit: usize, capacity: usize) -> Self {
        // Charge actual index capacity, including unused slots retained after eviction.
        let capacity = capacity.min(limit / std::mem::size_of::<Entry<K, V>>());
        let entries = VecDeque::with_capacity(capacity);
        let bytes = entries.capacity() * std::mem::size_of::<Entry<K, V>>();
        Self {
            entries,
            bytes,
            limit,
            capacity,
        }
    }

    pub(crate) fn get(&mut self, fingerprint: u64, equals: impl Fn(&K) -> bool) -> Option<Arc<V>> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.fingerprint == fingerprint && equals(&entry.key))?;
        let entry = self.entries.remove(index)?;
        let value = Arc::clone(&entry.value);
        self.entries.push_back(entry);
        Some(value)
    }

    pub(crate) fn entry_bytes(heap_bytes: usize) -> Option<usize> {
        // Index allocation is charged at construction; charge the Arc allocation here.
        heap_bytes.checked_add(std::mem::size_of::<V>() + 2 * std::mem::size_of::<usize>())
    }

    #[cfg(test)]
    pub(crate) fn one_entry_budget(heap_bytes: usize) -> usize {
        Self::entry_bytes(heap_bytes).unwrap() + std::mem::size_of::<Entry<K, V>>()
    }

    /// Heap bytes include every owned key/value allocation, including encoded copies.
    /// Oversized values remain usable by the caller but never displace retained entries.
    pub(crate) fn insert(&mut self, fingerprint: u64, key: K, value: Arc<V>, heap_bytes: usize) {
        let Some(bytes) = Self::entry_bytes(heap_bytes) else {
            return;
        };
        let metadata = self.entries.capacity() * std::mem::size_of::<Entry<K, V>>();
        if self.capacity == 0 || bytes > self.limit.saturating_sub(metadata) {
            return;
        }
        while self.bytes > self.limit - bytes || self.entries.len() >= self.capacity {
            if let Some(entry) = self.entries.pop_front() {
                self.bytes -= entry.bytes;
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back(Entry {
            fingerprint,
            key,
            value,
            bytes,
        });
    }
}

pub(super) struct MaskCache(Cache<(u32, u32, Vec<u8>), Vec<u8>>);

impl Default for MaskCache {
    fn default() -> Self {
        Self(Cache::new(8 * 1024 * 1024, 2048))
    }
}

impl MaskCache {
    #[cfg(test)]
    fn get(
        &mut self,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<Arc<Vec<u8>>, super::RenderError> {
        self.get_with_status(width, height, pixels)
            .map(|(rgba, _hit)| rgba)
    }

    /// `hit` distinguishes reused RGBA from a newly converted mask, including
    /// successful conversions which exceed the retention budget.
    pub(super) fn get_with_status(
        &mut self,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<(Arc<Vec<u8>>, bool), super::RenderError> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err(super::RenderError::Capacity(
                "glyph dimensions exceed upload limit",
            ));
        }
        let length = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n <= 64 * 1024 * 1024)
            .ok_or(super::RenderError::Capacity("glyph exceeds byte limit"))?;
        if pixels.len() != length / 4 {
            return Err(super::RenderError::Capacity("invalid glyph mask length"));
        }
        let mut hasher = DefaultHasher::new();
        (width, height, pixels).hash(&mut hasher);
        let fingerprint = hasher.finish();
        if let Some(rgba) = self.0.get(fingerprint, |key| {
            key.0 == width && key.1 == height && key.2 == pixels
        }) {
            return Ok((rgba, true));
        }
        let rgba = Arc::new(
            pixels
                .iter()
                .flat_map(|alpha| [255, 255, 255, *alpha])
                .collect::<Vec<_>>(),
        );
        let key = (width, height, pixels.to_vec());
        let bytes = key.2.capacity() + rgba.capacity();
        self.0.insert(fingerprint, key, Arc::clone(&rgba), bytes);
        Ok((rgba, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_status_reports_conversion_then_retained_reuse() {
        let mut cache = MaskCache::default();
        let (first, hit) = cache.get_with_status(1, 1, &[10]).unwrap();
        assert!(!hit);
        let (second, hit) = cache.get_with_status(1, 1, &[10]).unwrap();
        assert!(hit);
        assert!(Arc::ptr_eq(&first, &second));
        let (_, hit) = cache.get_with_status(1, 1, &[20]).unwrap();
        assert!(!hit);
        let mut disabled = MaskCache(Cache::new(0, 0));
        for _ in 0..2 {
            assert!(!disabled.get_with_status(1, 1, &[10]).unwrap().1);
        }
    }

    #[test]
    fn masks_reuse_content_without_pointer_identity() {
        let mut cache = MaskCache::default();
        let first = cache.get(2, 1, &[10, 20]).unwrap();
        let repeated = cache.get(2, 1, &[10, 20]).unwrap();
        assert!(Arc::ptr_eq(&first, &repeated));
        assert_eq!(first.as_slice(), [255, 255, 255, 10, 255, 255, 255, 20]);
        let changed = cache.get(2, 1, &[30, 40]).unwrap();
        assert_eq!(changed[3], 30);
        assert!(!Arc::ptr_eq(&first, &changed));
        let reshaped = cache.get(1, 2, &[10, 20]).unwrap();
        assert!(!Arc::ptr_eq(&first, &reshaped));
        assert!(cache.get(0, 1, &[]).is_err());
        assert!(cache.get(u32::MAX, 1, &[]).is_err());
        assert!(cache.get(1, 1, &[]).is_err());
    }

    #[test]
    fn collisions_require_exact_equality_and_hits_refresh_lru() {
        let mut cache = Cache::new(4096, 2);
        cache.insert(7, 1, Arc::new(10), 0);
        cache.insert(7, 2, Arc::new(20), 0);
        assert!(cache.get(7, |k| *k == 3).is_none());
        assert_eq!(*cache.get(7, |k| *k == 1).unwrap(), 10);
        cache.insert(7, 3, Arc::new(30), 0);
        assert!(cache.get(7, |k| *k == 2).is_none());
        assert_eq!(*cache.get(7, |k| *k == 3).unwrap(), 30);
    }

    #[test]
    fn byte_and_entry_bounds_include_keys_and_oversize_does_not_flush() {
        let overhead = Cache::<u8, u8>::entry_bytes(0).unwrap();
        let metadata = 2 * std::mem::size_of::<Entry<u8, u8>>();
        let limit = metadata + overhead + 8;
        let mut cache: Cache<u8, u8> = Cache::new(limit, 2);
        cache.insert(1, 1, Arc::new(1), 8);
        assert_eq!(cache.bytes, limit);
        cache.insert(2, 2, Arc::new(2), 9);
        assert!(cache.get(1, |k| *k == 1).is_some());
        cache.insert(2, 2, Arc::new(2), 8);
        assert!(cache.get(1, |k| *k == 1).is_none());
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, limit);
        let mut disabled = Cache::new(4096, 0);
        disabled.insert(1, 1, Arc::new(1), 0);
        assert!(disabled.entries.is_empty());
    }
}
