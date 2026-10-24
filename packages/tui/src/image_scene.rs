//! Frame-to-frame reconciliation for protocol-neutral image contributions.

use std::collections::{BTreeMap, BTreeSet};

use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePlacement};

#[cfg(test)]
thread_local! {
    // Actual active snapshots and delta constructions on this test thread.
    static RECONCILE_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Changes produced by reconciling one rendered frame with the active image scene.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImageSceneDelta {
    /// Images newly added or changed by this frame, in stable key order.
    pub upserted: Vec<ImagePlacement>,
    /// Keys removed explicitly or because a frame-scoped image became stale.
    pub removed: Vec<ImageKey>,
}

/// Protocol-neutral visible image scene with composer-local retained sources.
///
/// Equality and serialization describe only visible placements. Hidden originals
/// and fragment provenance belong to the authoritative composer, are preserved by
/// cloning, and are not transferred to audience snapshots as resource ownership.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImageScene {
    active: BTreeMap<ImageKey, ImagePlacement>,
    #[cfg_attr(feature = "serde", serde(skip))]
    sources: BTreeMap<ImageKey, ImagePlacement>,
    #[cfg_attr(feature = "serde", serde(skip))]
    visibility_sources: BTreeMap<ImageKey, ImageKey>,
}

impl PartialEq for ImageScene {
    fn eq(&self, other: &Self) -> bool {
        self.active == other.active
    }
}

impl Eq for ImageScene {}

impl ImageScene {
    /// Return active image placements in stable key order.
    #[must_use]
    pub fn placements(&self) -> impl ExactSizeIterator<Item = &ImagePlacement> {
        self.active.values()
    }

    /// Return the active placement for `key`.
    #[must_use]
    pub fn get(&self, key: &ImageKey) -> Option<&ImagePlacement> {
        self.active.get(key)
    }

    /// Retained original placements, including hidden persistent images.
    #[must_use]
    pub fn source_placements(&self) -> impl ExactSizeIterator<Item = &ImagePlacement> {
        self.sources.values()
    }

    /// Original caller identity for a visible derived fragment.
    #[must_use]
    pub fn source_key(&self, visible: &ImageKey) -> Option<&ImageKey> {
        self.visibility_sources.get(visible)
    }

    /// Clone retained placements that do not intersect any damaged region.
    #[must_use]
    pub fn contributions_outside(
        &self,
        regions: &[crate::geometry::Rect],
    ) -> Vec<ImageContribution> {
        self.sources
            .values()
            .filter(|placement| {
                !regions
                    .iter()
                    .any(|region| !placement.destination.intersection(*region).is_empty())
            })
            .cloned()
            .map(ImageContribution::Present)
            .collect()
    }

    /// Reconcile contributions emitted by one complete rendered frame.
    ///
    /// Frame-scoped images omitted from `contributions` are removed. Persistent
    /// images remain until replaced or explicitly removed. If a key occurs more
    /// than once, contributions are applied in render order and the last one
    /// determines the final scene.
    pub fn reconcile(&mut self, contributions: &[ImageContribution]) -> ImageSceneDelta {
        #[cfg(test)]
        RECONCILE_WORK.with(|work| work.set((work.get().0 + 1, work.get().1)));
        let previous = self.active.clone();
        self.apply_contributions(contributions);
        #[cfg(test)]
        RECONCILE_WORK.with(|work| work.set((work.get().0, work.get().1 + 1)));
        let upserted = self
            .active
            .iter()
            .filter(|(key, placement)| previous.get(*key) != Some(*placement))
            .map(|(_, placement)| placement.clone())
            .collect();
        let removed = previous
            .keys()
            .filter(|key| !self.active.contains_key(*key))
            .cloned()
            .collect();

        ImageSceneDelta { upserted, removed }
    }
    fn apply_contributions(&mut self, contributions: &[ImageContribution]) {
        if self.sources.is_empty() && self.visibility_sources.is_empty() {
            // Older serialized scenes contain only the original active map.
            self.sources.clone_from(&self.active);
        }
        let contributed_keys = contributions
            .iter()
            .filter_map(|contribution| match contribution {
                ImageContribution::Present(placement) => Some(placement.key.clone()),
                ImageContribution::Remove(_) => None,
            })
            .collect::<BTreeSet<_>>();

        self.sources.retain(|key, placement| {
            placement.lifecycle == ImageLifecycle::Persistent || contributed_keys.contains(key)
        });

        for contribution in contributions {
            match contribution {
                ImageContribution::Present(placement) => {
                    self.sources
                        .insert(placement.key.clone(), placement.clone());
                }
                ImageContribution::Remove(key) => {
                    self.sources.remove(key);
                }
            }
        }

        self.active.clone_from(&self.sources);
        self.visibility_sources.clear();
    }

    fn snapshot_active(&mut self) -> BTreeMap<ImageKey, ImagePlacement> {
        if self.sources.is_empty() && self.visibility_sources.is_empty() {
            // Bootstrap legacy serialized active-only scenes before detaching
            // their payloads; normal composed scenes already own originals.
            self.sources.clone_from(&self.active);
        }
        std::mem::take(&mut self.active)
    }

    /// Reconcile original image lifetime, then project opaque paint-order clips.
    /// Hidden persistent sources remain retained; only visibility fragments are
    /// retired. Rectangles partition existing pieces, never duplicate pixels.
    pub(crate) fn reconcile_occluded(
        &mut self,
        contributions: &[ImageContribution],
        occlusions: &[(usize, crate::geometry::Rect)],
    ) -> ImageSceneDelta {
        #[cfg(test)]
        RECONCILE_WORK.with(|work| work.set((work.get().0 + 1, work.get().1)));
        let previous = self.snapshot_active();
        let previous_sources = self.visibility_sources.clone();
        self.apply_contributions(contributions);
        let mut painted_at = BTreeMap::new();
        for (index, contribution) in contributions.iter().enumerate() {
            if let ImageContribution::Present(placement) = contribution {
                painted_at.insert(placement.key.clone(), index);
            }
        }
        self.active.clear();
        for (key, source) in &self.sources {
            let mut pieces = vec![source.clip.intersection(source.destination)];
            for (boundary, area) in occlusions {
                if painted_at.get(key).is_none_or(|index| index < boundary) {
                    pieces = pieces
                        .into_iter()
                        .flat_map(|piece| subtract_rect(piece, *area))
                        .collect();
                }
            }
            pieces.retain(|piece| !piece.is_empty());
            pieces.sort_by_key(|piece| (piece.y, piece.x, piece.height, piece.width));
            if pieces.len() == 1 && pieces[0] == source.clip.intersection(source.destination) {
                self.active.insert(key.clone(), source.clone());
                continue;
            }
            for clip in pieces {
                let base = format!(
                    "bmux-clip:{}:{}:{},{},{},{}",
                    key.as_str().len(),
                    key.as_str(),
                    clip.x,
                    clip.y,
                    clip.width,
                    clip.height
                );
                let mut fragment_key = ImageKey::new(base.clone());
                let mut suffix = 0_usize;
                while self.sources.contains_key(&fragment_key)
                    || self.active.contains_key(&fragment_key)
                    || previous.contains_key(&fragment_key)
                        && previous_sources.get(&fragment_key) != Some(key)
                {
                    suffix = suffix.saturating_add(1);
                    fragment_key = ImageKey::new(format!("{base}:{suffix}"));
                }
                let mut fragment = source.clone();
                fragment.key = fragment_key.clone();
                fragment.clip = clip;
                fragment.lifecycle = ImageLifecycle::Frame;
                self.visibility_sources
                    .insert(fragment_key.clone(), key.clone());
                self.active.insert(fragment_key, fragment);
            }
        }
        #[cfg(test)]
        RECONCILE_WORK.with(|work| work.set((work.get().0, work.get().1 + 1)));
        ImageSceneDelta {
            upserted: self
                .active
                .iter()
                .filter(|(key, placement)| previous.get(*key) != Some(*placement))
                .map(|(_, placement)| placement.clone())
                .collect(),
            removed: previous
                .keys()
                .filter(|key| !self.active.contains_key(*key))
                .cloned()
                .collect(),
        }
    }
}

/// Disjoint rectangular complement shared by staged metadata and image projection.
pub(crate) fn subtract_rect(
    area: crate::geometry::Rect,
    occluded: crate::geometry::Rect,
) -> Vec<crate::geometry::Rect> {
    use crate::geometry::Rect;
    let cut = area.intersection(occluded);
    if cut.is_empty() {
        return (!area.is_empty()).then_some(area).into_iter().collect();
    }
    [
        Rect::new(area.x, area.y, area.width, cut.y.saturating_sub(area.y)),
        Rect::new(
            area.x,
            cut.bottom(),
            area.width,
            area.bottom().saturating_sub(cut.bottom()),
        ),
        Rect::new(area.x, cut.y, cut.x.saturating_sub(area.x), cut.height),
        Rect::new(
            cut.right(),
            cut.y,
            area.right().saturating_sub(cut.right()),
            cut.height,
        ),
    ]
    .into_iter()
    .filter(|piece| !piece.is_empty())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{ImageScene, ImageSceneDelta};
    use crate::geometry::Rect;
    use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};
    use std::collections::BTreeSet;

    fn placement(key: &str, x: u16, lifecycle: ImageLifecycle) -> ImagePlacement {
        ImagePlacement {
            fit: crate::image::ImageFit::Stretch,
            allocation: None,
            key: ImageKey::new(key),
            payload: ImagePayload::Png {
                bytes: vec![1, 2, 3],
                width: 1,
                height: 1,
            },
            destination: Rect::new(x, 0, 1, 1),
            clip: Rect::new(0, 0, 80, 24),
            lifecycle,
        }
    }

    fn payload_ptr(image: &ImagePlacement) -> *const u8 {
        match &image.payload {
            ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.as_ptr(),
        }
    }

    #[test]
    fn occluded_previous_snapshot_moves_payload_allocation() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "warm",
            0,
            ImageLifecycle::Persistent,
        ))]);
        let key = ImageKey::new("warm");
        let original = payload_ptr(&scene.active[&key]);
        let previous = scene.snapshot_active();
        assert_eq!(
            payload_ptr(&previous[&key]),
            original,
            "staged previous snapshot must move, not copy, encoded payload bytes"
        );
    }

    #[test]
    fn occluded_moved_snapshot_preserves_legacy_duplicate_lifetime_and_fragments() {
        let persistent = placement("persistent", 0, ImageLifecycle::Persistent);
        let stale = placement("stale", 4, ImageLifecycle::Frame);
        let mut legacy = ImageScene::default();
        legacy
            .active
            .insert(persistent.key.clone(), persistent.clone());
        legacy.active.insert(stale.key.clone(), stale);
        let mut canonical = ImageScene::default();
        canonical.reconcile(&[
            ImageContribution::Present(persistent),
            ImageContribution::Present(placement("stale", 4, ImageLifecycle::Frame)),
        ]);
        let mut replacement = placement("dup", 2, ImageLifecycle::Persistent);
        replacement.destination = Rect::new(2, 0, 4, 1);
        replacement.clip = replacement.destination;
        let contributions = [
            ImageContribution::Present(placement("dup", 1, ImageLifecycle::Frame)),
            ImageContribution::Remove(ImageKey::new("dup")),
            ImageContribution::Present(replacement),
        ];
        let occlusions = [(3, Rect::new(3, 0, 1, 1))];
        let original_legacy = legacy.clone();
        let delta = legacy.reconcile_occluded(&contributions, &occlusions);
        let expected = canonical.reconcile_occluded(&contributions, &occlusions);
        assert_eq!(delta, expected);
        assert_eq!(legacy, canonical);
        assert_eq!(legacy.visibility_sources, canonical.visibility_sources);
        assert_eq!(legacy.sources, canonical.sources);
        assert_eq!(
            original_legacy.active.len(),
            2,
            "staging must leave rollback original untouched"
        );
        assert_eq!(original_legacy.sources.len(), 0);
        assert!(delta.removed.contains(&ImageKey::new("stale")));
        assert!(!legacy.active.contains_key(&ImageKey::new("stale")));
        assert_eq!(
            legacy.sources[&ImageKey::new("dup")].lifecycle,
            ImageLifecycle::Persistent
        );
        let warm = legacy.reconcile_occluded(&contributions, &occlusions);
        assert_eq!(warm, ImageSceneDelta::default());
        assert_eq!(
            legacy.reconcile_occluded(&[], &[]),
            canonical.reconcile_occluded(&[], &[])
        );
        assert_eq!(legacy, canonical);
        assert_eq!(legacy.sources.len(), 2);
    }

    #[test]
    fn occluded_reconcile_builds_only_final_snapshot_and_delta() {
        for bytes in [4, 65_536] {
            let mut image = placement("warm", 0, ImageLifecycle::Persistent);
            image.payload = ImagePayload::Pixels {
                bytes: vec![7; bytes],
                width: 1,
                height: u32::try_from(bytes / 4).unwrap(),
                format: crate::image::ImagePixelFormat::Rgba8,
            };
            let mut scene = ImageScene::default();
            scene.reconcile(&[ImageContribution::Present(image.clone())]);
            super::RECONCILE_WORK.with(|work| work.set((0, 0)));
            let delta = scene.reconcile_occluded(&[ImageContribution::Present(image.clone())], &[]);
            assert_eq!(delta, ImageSceneDelta::default());
            assert_eq!(scene.placements().next(), Some(&image));
            assert_eq!(super::RECONCILE_WORK.with(std::cell::Cell::get), (1, 1));
        }
    }

    #[test]
    fn contribution_application_preserves_public_and_occluded_semantics() {
        let persistent = placement("persistent", 0, ImageLifecycle::Persistent);
        let frame = placement("frame", 1, ImageLifecycle::Frame);
        let last = placement("frame", 2, ImageLifecycle::Frame);
        // Model deserialization: only active placements, no composer-local maps.
        let legacy = ImageScene {
            active: [
                (persistent.key.clone(), persistent.clone()),
                (frame.key.clone(), frame.clone()),
            ]
            .into_iter()
            .collect(),
            ..ImageScene::default()
        };
        for contributions in [
            vec![],
            vec![
                ImageContribution::Present(frame.clone()),
                ImageContribution::Present(last.clone()),
            ],
            vec![
                ImageContribution::Present(last.clone()),
                ImageContribution::Remove(frame.key.clone()),
            ],
            vec![
                ImageContribution::Remove(frame.key.clone()),
                ImageContribution::Present(last.clone()),
            ],
            vec![ImageContribution::Remove(persistent.key.clone())],
        ] {
            let mut public = legacy.clone();
            let mut occluded = legacy.clone();
            let expected = public.reconcile(&contributions);
            assert_eq!(occluded.reconcile_occluded(&contributions, &[]), expected);
            assert_eq!(occluded, public);
            assert_eq!(occluded.sources, public.sources);
            assert_eq!(occluded.visibility_sources, public.visibility_sources);
        }
        let mut scene = legacy;
        let delta = scene.reconcile_occluded(
            &[
                ImageContribution::Present(frame),
                ImageContribution::Present(last.clone()),
            ],
            &[(2, Rect::new(0, 0, 1, 1))],
        );
        assert_eq!(delta.removed, vec![persistent.key.clone()]);
        assert_eq!(delta.upserted, vec![last.clone()]);
        assert_eq!(scene.placements().cloned().collect::<Vec<_>>(), vec![last]);
        assert!(scene.sources.contains_key(&persistent.key));
        scene.reconcile_occluded(&[], &[]);
        assert_eq!(scene.placements().next(), Some(&persistent));
    }

    #[test]
    fn visible_projection_equality_does_not_transfer_hidden_source_ownership() {
        let mut composer = ImageScene::default();
        let mut source = placement("original", 0, ImageLifecycle::Persistent);
        source.destination = Rect::new(0, 0, 8, 4);
        source.clip = source.destination;
        composer.reconcile_occluded(
            &[ImageContribution::Present(source.clone())],
            &[(1, Rect::new(3, 1, 2, 2))],
        );
        let audience = ImageScene {
            active: composer.active.clone(),
            ..ImageScene::default()
        };
        assert_eq!(composer, audience);
        assert_eq!(composer.source_placements().len(), 1);
        assert_eq!(audience.source_placements().len(), 0);
        assert!(
            composer
                .placements()
                .all(|image| composer.source_key(&image.key) == Some(&source.key))
        );
        assert!(
            audience
                .placements()
                .all(|image| audience.source_key(&image.key).is_none())
        );
        let mut retained = composer.clone();
        retained.reconcile_occluded(&[], &[]);
        assert_eq!(retained.get(&source.key), Some(&source));
        assert_ne!(retained, audience);
        composer.reconcile_occluded(&[], &[(0, source.destination)]);
        assert_eq!(composer, ImageScene::default());
        assert_eq!(composer.source_placements().next(), Some(&source));
    }

    #[test]
    fn persistent_sources_hide_reveal_move_and_remove_without_lifetime_loss() {
        let mut scene = ImageScene::default();
        let mut source = placement("persistent", 0, ImageLifecycle::Persistent);
        source.destination = Rect::new(0, 0, 8, 4);
        source.clip = source.destination;
        scene.reconcile_occluded(&[ImageContribution::Present(source.clone())], &[]);
        let hidden = scene.reconcile_occluded(&[], &[(0, source.destination)]);
        assert_eq!(hidden.removed, [source.key.clone()]);
        assert_eq!(scene.placements().len(), 0);
        assert_eq!(scene.source_placements().next(), Some(&source));
        let revealed = scene.reconcile_occluded(&[], &[]);
        assert_eq!(revealed.upserted, [source.clone()]);
        let cut = Rect::new(3, 1, 2, 2);
        let delta = scene.reconcile_occluded(&[], &[(0, cut)]);
        assert_eq!(delta.upserted.len(), 4);
        let keys = scene
            .placements()
            .map(|image| image.key.clone())
            .collect::<Vec<_>>();
        for image in scene.placements() {
            assert_eq!(scene.source_key(&image.key), Some(&source.key));
            assert_eq!(image.payload, source.payload);
            assert_eq!(image.destination, source.destination);
            assert!(image.clip.intersection(cut).is_empty());
        }
        assert_eq!(
            scene.reconcile_occluded(&[], &[(0, cut)]),
            ImageSceneDelta::default()
        );
        source.destination = Rect::new(0, 0, 2, 2);
        source.clip = source.destination;
        let moved = scene.reconcile_occluded(&[ImageContribution::Present(source.clone())], &[]);
        assert_eq!(moved.removed, keys);
        assert_eq!(moved.upserted, [source.clone()]);
        scene.reconcile_occluded(&[ImageContribution::Remove(source.key)], &[]);
        assert_eq!(scene.source_placements().len(), 0);
        assert_eq!(scene.placements().len(), 0);
    }

    #[test]
    fn nested_occlusions_partition_cells_and_respect_foreground_paint_order() {
        let mut scene = ImageScene::default();
        let mut background = placement("background", 0, ImageLifecycle::Frame);
        background.destination = Rect::new(0, 0, 12, 8);
        background.clip = background.destination;
        let mut foreground = background.clone();
        foreground.key = ImageKey::new("foreground");
        foreground.destination = Rect::new(4, 2, 4, 4);
        foreground.clip = foreground.destination;
        let cuts = [(1, Rect::new(2, 1, 8, 6)), (2, Rect::new(5, 3, 2, 2))];
        scene.reconcile_occluded(
            &[
                ImageContribution::Present(background),
                ImageContribution::Present(foreground),
            ],
            &cuts,
        );
        let mut covered = BTreeSet::new();
        for image in scene.placements() {
            for y in image.clip.y..image.clip.bottom() {
                for x in image.clip.x..image.clip.right() {
                    assert!(covered.insert((x, y)), "overlapping visible fragments");
                }
            }
        }
        assert!(scene.placements().len() <= 8);
        assert!(!covered.contains(&(5, 3)));
        assert!(covered.contains(&(4, 2)));
        assert_eq!(scene.source_placements().len(), 2);
    }

    #[test]
    fn adds_updates_and_removes_frame_scoped_images() {
        let mut scene = ImageScene::default();
        let initial = placement("diagram", 1, ImageLifecycle::Frame);
        let delta = scene.reconcile(&[ImageContribution::Present(initial.clone())]);
        assert_eq!(delta.upserted, [initial]);
        assert!(delta.removed.is_empty());

        let updated = placement("diagram", 2, ImageLifecycle::Frame);
        let delta = scene.reconcile(&[ImageContribution::Present(updated.clone())]);
        assert_eq!(delta.upserted, [updated]);
        assert!(delta.removed.is_empty());

        let delta = scene.reconcile(&[]);
        assert!(delta.upserted.is_empty());
        assert_eq!(delta.removed, [ImageKey::new("diagram")]);
        assert_eq!(scene.placements().len(), 0);
    }

    #[test]
    fn persistent_images_survive_omission_until_explicitly_removed() {
        let mut scene = ImageScene::default();
        scene.reconcile(&[ImageContribution::Present(placement(
            "badge",
            1,
            ImageLifecycle::Persistent,
        ))]);

        let unchanged = scene.reconcile(&[]);
        assert_eq!(scene.placements().len(), 1);
        assert_eq!(unchanged, ImageSceneDelta::default());

        let removed = scene.reconcile(&[ImageContribution::Remove(ImageKey::new("badge"))]);
        assert_eq!(removed.removed, [ImageKey::new("badge")]);
        assert_eq!(scene.placements().len(), 0);
    }

    #[test]
    fn last_contribution_for_a_key_wins() {
        let mut scene = ImageScene::default();
        let first = placement("image", 1, ImageLifecycle::Frame);
        let last = placement("image", 5, ImageLifecycle::Frame);

        scene.reconcile(&[
            ImageContribution::Present(first),
            ImageContribution::Remove(ImageKey::new("image")),
            ImageContribution::Present(last.clone()),
        ]);

        assert_eq!(scene.get(&ImageKey::new("image")), Some(&last));
    }

    #[test]
    fn retained_contributions_outside_damage_use_destination_not_shared_clip() {
        let mut scene = ImageScene::default();
        let left = placement("left", 1, ImageLifecycle::Frame);
        let right = placement("right", 5, ImageLifecycle::Frame);
        scene.reconcile(&[
            ImageContribution::Present(left),
            ImageContribution::Present(right),
        ]);

        let retained = scene.contributions_outside(&[Rect::new(0, 0, 3, 3)]);

        assert_eq!(retained.len(), 1);
        assert!(matches!(
            &retained[0],
            ImageContribution::Present(placement) if placement.key.as_str() == "right"
        ));
    }
}
