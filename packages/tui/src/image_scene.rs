//! Frame-to-frame reconciliation for protocol-neutral image contributions.

use std::collections::{BTreeMap, BTreeSet};

use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePlacement};

/// Changes produced by reconciling one rendered frame with the active image scene.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageSceneDelta {
    /// Images newly added or changed by this frame, in stable key order.
    pub upserted: Vec<ImagePlacement>,
    /// Keys removed explicitly or because a frame-scoped image became stale.
    pub removed: Vec<ImageKey>,
}

/// Active protocol-neutral image scene retained across rendered frames.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageScene {
    active: BTreeMap<ImageKey, ImagePlacement>,
}

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

    /// Clone retained placements that do not intersect any damaged region.
    #[must_use]
    pub fn contributions_outside(
        &self,
        regions: &[crate::geometry::Rect],
    ) -> Vec<ImageContribution> {
        self.active
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
        let contributed_keys = contributions
            .iter()
            .filter_map(|contribution| match contribution {
                ImageContribution::Present(placement) => Some(placement.key.clone()),
                ImageContribution::Remove(_) => None,
            })
            .collect::<BTreeSet<_>>();

        self.active.retain(|key, placement| {
            placement.lifecycle == ImageLifecycle::Persistent || contributed_keys.contains(key)
        });

        for contribution in contributions {
            match contribution {
                ImageContribution::Present(placement) => {
                    self.active.insert(placement.key.clone(), placement.clone());
                }
                ImageContribution::Remove(key) => {
                    self.active.remove(key);
                }
            }
        }

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
}

#[cfg(test)]
mod tests {
    use super::{ImageScene, ImageSceneDelta};
    use crate::geometry::Rect;
    use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};

    fn placement(key: &str, x: u16, lifecycle: ImageLifecycle) -> ImagePlacement {
        ImagePlacement {
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
