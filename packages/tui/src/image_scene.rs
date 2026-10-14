//! Frame-to-frame reconciliation for protocol-neutral image contributions.

use std::collections::{BTreeMap, BTreeSet};

use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePlacement};

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
        let previous = self.active.clone();
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
    /// Reconcile original image lifetime, then project opaque paint-order clips.
    /// Hidden persistent sources remain retained; only visibility fragments are
    /// retired. Rectangles partition existing pieces, never duplicate pixels.
    pub(crate) fn reconcile_occluded(
        &mut self,
        contributions: &[ImageContribution],
        occlusions: &[(usize, crate::geometry::Rect)],
    ) -> ImageSceneDelta {
        let previous = self.active.clone();
        let previous_sources = self.visibility_sources.clone();
        self.reconcile(contributions);
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
