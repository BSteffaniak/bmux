//! What changed between two decoration scenes, as a decoration view pushes it to its connection.

use crate::scene_delta::DecorationSceneDelta;
use crate::scene_protocol::DecorationScene;

/// The delta that takes a connection from `sent` (the scene it was last sent, if any) to
/// `current`: the whole scene when nothing was sent, only the surfaces that differ otherwise.
/// `None` when `current` is the scene already sent.
#[must_use]
pub fn scene_delta(
    sent: Option<&DecorationScene>,
    current: &DecorationScene,
) -> Option<DecorationSceneDelta> {
    let Some(sent) = sent else {
        return Some(DecorationSceneDelta {
            base_revision: 0,
            revision: current.revision,
            reset: true,
            surfaces: current.surfaces.clone(),
            removed: Vec::new(),
            animation: current.animation.clone(),
            input_hooks: current.input_hooks.clone(),
            visual_adapters: current.visual_adapters.clone(),
        });
    };
    if sent.revision == current.revision {
        return None;
    }
    let surfaces = current
        .surfaces
        .iter()
        .filter(|(id, surface)| sent.surfaces.get(id) != Some(surface))
        .map(|(id, surface)| (*id, surface.clone()))
        .collect();
    let removed = sent
        .surfaces
        .keys()
        .filter(|id| !current.surfaces.contains_key(id))
        .copied()
        .collect();
    Some(DecorationSceneDelta {
        base_revision: sent.revision,
        revision: current.revision,
        reset: false,
        surfaces,
        removed,
        animation: current.animation.clone(),
        input_hooks: current.input_hooks.clone(),
        visual_adapters: current.visual_adapters.clone(),
    })
}

/// The scene after `delta`, on `held` (the scene it was computed on, unless it is a reset);
/// `None` when it does not apply there.
#[must_use]
pub fn apply_scene_delta(
    held: Option<&DecorationScene>,
    delta: DecorationSceneDelta,
) -> Option<DecorationScene> {
    let mut surfaces = if delta.reset {
        std::collections::BTreeMap::new()
    } else {
        let held = held.filter(|held| held.revision == delta.base_revision)?;
        let mut surfaces = held.surfaces.clone();
        for id in &delta.removed {
            surfaces.remove(id);
        }
        surfaces
    };
    surfaces.extend(delta.surfaces);
    Some(DecorationScene {
        revision: delta.revision,
        surfaces,
        animation: delta.animation,
        input_hooks: delta.input_hooks,
        visual_adapters: delta.visual_adapters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_protocol::{Rect, SurfaceDecoration};
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn surface(id: Uuid, x: u16) -> SurfaceDecoration {
        let rect = Rect {
            x,
            y: 0,
            w: 4,
            h: 4,
        };
        SurfaceDecoration {
            surface_id: id,
            rect: rect.clone(),
            content_rect: rect,
            before_content_paint_commands: Vec::new(),
            paint_commands: Vec::new(),
            interactive_regions: Vec::new(),
        }
    }

    fn scene(revision: u64, surfaces: &[(Uuid, u16)]) -> DecorationScene {
        DecorationScene {
            revision,
            surfaces: surfaces
                .iter()
                .map(|(id, x)| (*id, surface(*id, *x)))
                .collect::<BTreeMap<_, _>>(),
            animation: None,
            input_hooks: Vec::new(),
            visual_adapters: Vec::new(),
        }
    }

    /// Nothing sent yet: the whole scene, as a reset. The same scene again: nothing. A later
    /// scene: only the surfaces that changed or went away, on the revision sent.
    #[test]
    fn deltas_carry_only_what_changed_since_what_was_sent() {
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let first = scene(1, &[(a, 0), (b, 4)]);
        let reset = scene_delta(None, &first).expect("a first scene is sent");
        assert!(reset.reset);
        assert_eq!(reset.surfaces.len(), 2);

        assert_eq!(scene_delta(Some(&first), &first), None);

        let next = scene(2, &[(a, 0), (c, 8)]);
        let delta = scene_delta(Some(&first), &next).expect("a newer scene is sent");
        assert!(!delta.reset);
        assert_eq!(delta.base_revision, 1);
        assert_eq!(delta.revision, 2);
        assert_eq!(delta.surfaces.keys().copied().collect::<Vec<_>>(), vec![c]);
        assert_eq!(delta.removed, vec![b]);
        assert_eq!(apply_scene_delta(Some(&first), delta.clone()), Some(next));
        assert_eq!(apply_scene_delta(None, delta), None, "not a reset");
        assert_eq!(apply_scene_delta(None, reset), Some(first));
    }
}
