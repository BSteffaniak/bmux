//! Derived raster retention, bounded by both payload bytes and entry count.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

use crate::{
    renderer::raster_cache::Cache,
    text::{RasterizedCell, TextStyle},
};

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub text: String,
    pub columns: u16,
    pub style: TextStyle,
    pub size: u32,
    pub phase: [u8; 2],
}

pub(crate) struct TextCache(Cache<Key, RasterizedCell>);

fn fingerprint(key: &Key) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

impl TextCache {
    pub(crate) fn new(limit: usize, capacity: usize) -> Self {
        Self(Cache::new(limit, capacity))
    }

    pub(crate) fn get(&mut self, key: &Key) -> Option<Arc<RasterizedCell>> {
        self.0.get(fingerprint(key), |candidate| candidate == key)
    }

    pub(crate) fn insert(&mut self, key: Key, value: Arc<RasterizedCell>) {
        // Charge allocation capacities, not lengths: keys, glyph storage, source
        // ranges, Arc headers and the fixed index are all part of the bound.
        let bytes = key.text.capacity()
            + value.glyphs.capacity() * std::mem::size_of::<crate::text::GlyphBitmap>()
            + value
                .glyphs
                .iter()
                .map(|glyph| glyph.pixels.capacity())
                .sum::<usize>();
        self.0.insert(fingerprint(&key), key, value, bytes);
    }

    pub(crate) fn usage(&self) -> (usize, usize) {
        self.0.usage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> Key {
        Key {
            text: text.to_owned(),
            columns: 1,
            style: TextStyle::default(),
            size: 16.0_f32.to_bits(),
            phase: [0; 2],
        }
    }

    fn value() -> Arc<RasterizedCell> {
        Arc::new(RasterizedCell {
            columns: 1,
            source_bytes: 1,
            glyphs: Vec::new(),
        })
    }

    #[test]
    fn entry_capacity_evicts_least_recently_used() {
        let mut cache = TextCache::new(4096, 2);
        cache.insert(key("a"), value());
        cache.insert(key("b"), value());
        assert!(cache.get(&key("a")).is_some());
        cache.insert(key("c"), value());
        assert!(cache.get(&key("b")).is_none());
        assert!(cache.get(&key("a")).is_some());
        assert_eq!(cache.usage().0, 2);
        assert!(cache.usage().1 <= 4096);
    }

    #[test]
    fn over_budget_entry_is_not_retained() {
        let mut cache = TextCache::new(1, 100);
        cache.insert(key("a"), value());
        assert_eq!(cache.usage(), (0, 0));
    }
}
