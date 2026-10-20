//! Exact, bounded host-content identities, independent of placement ownership.
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::frozen::FrozenImage;
use crate::model::KittyFormat;

#[derive(Clone, Debug)]
struct Content {
    format: KittyFormat,
    width: u32,
    height: u32,
    bytes: ContentBytes,
    generation: Arc<()>,
}

#[derive(Clone, Debug)]
enum ContentBytes {
    Checked(Arc<[u8]>),
    Frozen(Arc<FrozenImage>),
}

impl ContentBytes {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Checked(bytes) => bytes,
            Self::Frozen(image) => image.wire().1,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Residency {
    entries: HashMap<u64, Content>,
    index: HashMap<u64, Vec<u64>>,
    // Only recent owners are useful for pre-paint protection; never retain an
    // unbounded history of caller placement identities.
    aliases: HashMap<u64, u64>,
    alias_order: VecDeque<u64>,
}

impl Residency {
    pub fn fingerprint(format: KittyFormat, width: u32, height: u32, bytes: &[u8]) -> u64 {
        #[cfg(test)]
        crate::model::test_work::update(|counts| counts.hash_bytes += bytes.len());
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        // KittyFormat intentionally needs no new public trait implementations.
        std::mem::discriminant(&format).hash(&mut hash);
        width.hash(&mut hash);
        height.hash(&mut hash);
        bytes.hash(&mut hash);
        hash.finish()
    }

    pub fn find(
        &self,
        hash: u64,
        format: KittyFormat,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Option<u64> {
        self.index.get(&hash)?.iter().copied().find(|id| {
            let content = &self.entries[id];
            content.format == format && content.width == width && content.height == height && {
                #[cfg(test)]
                crate::model::test_work::update(|counts| counts.equality_bytes += bytes.len());
                content.bytes.bytes() == bytes
            }
        })
    }

    pub fn insert(
        &mut self,
        id: u64,
        hash: u64,
        format: KittyFormat,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) {
        self.entries.insert(
            id,
            Content {
                format,
                width,
                height,
                bytes: ContentBytes::Checked(Arc::from(bytes)),
                generation: Arc::new(()),
            },
        );
        self.index.entry(hash).or_default().push(id);
    }

    pub fn find_frozen(&self, frozen: &FrozenImage) -> Option<u64> {
        let (id, generation) = frozen.binding()?;
        let content = self.entries.get(&id)?;
        Arc::ptr_eq(&generation, &content.generation).then_some(id)
    }

    pub fn insert_frozen(&mut self, id: u64, hash: u64, frozen: &Arc<FrozenImage>) {
        let (format, _, width, height) = frozen.wire();
        self.entries.insert(
            id,
            Content {
                format,
                width,
                height,
                bytes: ContentBytes::Frozen(Arc::clone(frozen)),
                generation: Arc::new(()),
            },
        );
        self.index.entry(hash).or_default().push(id);
    }

    pub fn bind(&self, id: u64, frozen: &FrozenImage) {
        frozen.bind(id, &self.entries[&id].generation);
    }

    pub fn alias(&mut self, owner: u64, content: u64) {
        self.alias_order.retain(|id| *id != owner);
        if self.aliases.len() >= 1024
            && !self.aliases.contains_key(&owner)
            && let Some(oldest) = self.alias_order.pop_front()
        {
            // Losing an inactive owner hint cannot lose content equality or a
            // placement. Hot owners are refreshed rather than clearing them all.
            self.aliases.remove(&oldest);
        }
        self.aliases.insert(owner, content);
        self.alias_order.push_back(owner);
    }

    pub fn has_owner(&self, owner: u64) -> bool {
        self.aliases.contains_key(&owner)
    }

    pub fn resolve(&self, owner: u64) -> u64 {
        self.aliases.get(&owner).copied().unwrap_or(owner)
    }

    pub fn remove(&mut self, id: u64) {
        self.entries.remove(&id);
        self.index.retain(|_, ids| {
            ids.retain(|entry| *entry != id);
            !ids.is_empty()
        });
        self.aliases.retain(|_, content| *content != id);
        self.alias_order
            .retain(|owner| self.aliases.contains_key(owner));
    }

    /// Physical equality-buffer bytes (Arc clones of a candidate share these).
    #[cfg(test)]
    pub fn retained_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|content| content.bytes.bytes().len())
            .sum()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_requires_exact_metadata_and_bytes() {
        let mut residency = Residency::default();
        residency.insert(1, 7, KittyFormat::Rgba, 1, 1, &[1, 2, 3, 4]);
        assert_eq!(
            residency.find(7, KittyFormat::Rgba, 1, 1, &[1, 2, 3, 4]),
            Some(1)
        );
        assert_eq!(
            residency.find(7, KittyFormat::Rgba, 1, 1, &[4, 3, 2, 1]),
            None
        );
        assert_eq!(
            residency.find(7, KittyFormat::Rgba, 2, 1, &[1, 2, 3, 4]),
            None
        );
        assert_eq!(
            residency.find(7, KittyFormat::Png, 1, 1, &[1, 2, 3, 4]),
            None
        );
        residency.alias(2, 1);
        residency.remove(1);
        assert_eq!(residency.resolve(2), 2);
        assert!(residency.index.is_empty());
    }

    fn frozen(value: u8) -> Arc<FrozenImage> {
        use crate::model::{
            ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition, ImageProtocol, PaneImage,
            PixelBuffer, PixelFormat,
        };
        FrozenImage::new(PaneImage {
            id: 1,
            protocol: ImageProtocol::KittyGraphics,
            position: ImagePosition { row: 0, col: 0 },
            cell_size: ImageCellSize::default(),
            pixel_size: ImagePixelSize {
                width: 1,
                height: 1,
            },
            payload: ImagePayload {
                raw: None,
                pixels: Some(PixelBuffer {
                    width: 1,
                    height: 1,
                    format: PixelFormat::Rgba8,
                    data: vec![value; 4],
                }),
            },
        })
    }

    #[test]
    fn frozen_generation_checks_candidate_branches_eviction_and_id_reuse() {
        let image = frozen(1);
        let mut committed = Residency::default();
        committed.insert_frozen(1, 7, &image);
        committed.bind(1, &image);
        let mut candidate = committed.clone();
        assert_eq!(candidate.find_frozen(&image), Some(1));
        candidate.remove(1);
        let replacement = frozen(2);
        candidate.insert_frozen(1, 7, &replacement);
        assert_eq!(candidate.find_frozen(&image), None);
        // Candidate failure must not invalidate the committed generation.
        assert_eq!(committed.find_frozen(&image), Some(1));
        candidate.bind(1, &replacement);
        assert_eq!(candidate.find_frozen(&replacement), Some(1));
        assert_eq!(committed.find_frozen(&replacement), None);
        candidate.clear();
        assert_eq!(candidate.find_frozen(&replacement), None);
    }

    #[test]
    fn frozen_new_handles_require_exact_collision_checked_canonicalization() {
        let first = frozen(1);
        let equal = frozen(1);
        let changed = frozen(2);
        let mut residency = Residency::default();
        residency.insert_frozen(1, 7, &first);
        assert_eq!(residency.find_frozen(&equal), None);
        let (_, bytes, _, _) = equal.wire();
        assert_eq!(residency.find(7, KittyFormat::Rgba, 1, 1, bytes), Some(1));
        residency.bind(1, &equal);
        assert_eq!(residency.find_frozen(&equal), Some(1));
        let (_, bytes, _, _) = changed.wire();
        assert_eq!(residency.find(7, KittyFormat::Rgba, 1, 1, bytes), None);
        // Residency shares storage rather than cloning a second equality buffer.
        let ContentBytes::Frozen(retained) = &residency.entries[&1].bytes else {
            panic!("frozen entry must share storage");
        };
        assert!(Arc::ptr_eq(retained, &first));
        assert_eq!(residency.retained_bytes(), 4);
    }

    #[test]
    fn alias_history_is_bounded_and_keeps_recent_owners() {
        let mut residency = Residency::default();
        for owner in 0..2048 {
            residency.alias(owner, 42);
        }
        assert_eq!(residency.aliases.len(), 1024);
        assert_eq!(residency.alias_order.len(), 1024);
        assert_eq!(residency.resolve(2047), 42);
        assert_eq!(residency.resolve(0), 0);
        residency.remove(42);
        assert!(residency.aliases.is_empty());
        assert!(residency.alias_order.is_empty());
    }
}
