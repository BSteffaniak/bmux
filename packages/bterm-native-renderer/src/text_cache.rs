//! Derived raster retention, bounded by both payload bytes and entry count.

use std::{collections::VecDeque, sync::Arc};

use crate::text::{RasterizedCell, TextStyle};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub text: String,
    pub columns: u16,
    pub style: TextStyle,
    pub size: u32,
}

pub(crate) struct TextCache {
    entries: VecDeque<(Key, Arc<RasterizedCell>, usize)>,
    bytes: usize,
    limit: usize,
    capacity: usize,
}

impl TextCache {
    pub(crate) fn new(limit: usize, capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
            capacity,
        }
    }

    pub(crate) fn get(&mut self, key: &Key) -> Option<Arc<RasterizedCell>> {
        let index = self.entries.iter().position(|(k, _, _)| k == key)?;
        let entry = self.entries.remove(index)?;
        let value = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: Key, value: Arc<RasterizedCell>) {
        let bytes = key.text.len()
            + std::mem::size_of::<RasterizedCell>()
            + value
                .glyphs
                .iter()
                .map(|g| std::mem::size_of_val(g) + g.pixels.len())
                .sum::<usize>();
        if bytes > self.limit || self.capacity == 0 {
            return;
        }
        while self.bytes + bytes > self.limit || self.entries.len() >= self.capacity {
            if let Some((_, _, size)) = self.entries.pop_front() {
                self.bytes -= size;
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back((key, value, bytes));
    }

    pub(crate) fn usage(&self) -> (usize, usize) {
        (self.entries.len(), self.bytes)
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
