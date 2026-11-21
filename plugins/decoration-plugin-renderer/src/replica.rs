//! The attach client's copy of its decoration view's scene, kept current from pushed deltas.
//!
//! Surfaces are held behind shared pointers: a delta replaces the pointers of the surfaces it
//! carries and keeps the others, so a new snapshot copies pointers, not paint commands, and the
//! renderer re-derives only the surfaces whose pointer changed.

use std::collections::BTreeMap;
use std::sync::Arc;

use bmux_plugin::PluginEventKind;
use bmux_scene_protocol::scene_delta::DecorationSceneDelta;
use bmux_scene_protocol::scene_protocol::{
    AnimationHint, DecorationScene, InputHook, SurfaceDecoration, VisualAdapterRequest,
};
use uuid::Uuid;

/// The kind of the attach client's [`SceneReplica`] state on its presentation bus.
pub const REPLICA_STATE_KIND: PluginEventKind =
    PluginEventKind::from_static("bmux.decoration.renderer/scene-replica");

/// A decoration scene with shared surfaces, as the attach client holds it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneReplica {
    pub revision: u64,
    pub surfaces: BTreeMap<Uuid, Arc<SurfaceDecoration>>,
    pub animation: Option<AnimationHint>,
    pub input_hooks: Arc<[InputHook]>,
    pub visual_adapters: Arc<[VisualAdapterRequest]>,
}

/// A delta that does not apply on the scene held: it was computed on revision `base`, the
/// replica holds `held`. The connection reads the whole scene again (which also restarts what
/// it is pushed from a whole scene).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SceneGap {
    pub held: u64,
    pub base: u64,
}

impl std::fmt::Display for SceneGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "decoration scene delta on revision {} does not apply on revision {}",
            self.base, self.held
        )
    }
}

impl std::error::Error for SceneGap {}

impl SceneReplica {
    /// The replica of a whole scene.
    #[must_use]
    pub fn from_scene(scene: DecorationScene) -> Self {
        Self {
            revision: scene.revision,
            surfaces: scene
                .surfaces
                .into_iter()
                .map(|(id, surface)| (id, Arc::new(surface)))
                .collect(),
            animation: scene.animation,
            input_hooks: scene.input_hooks.into(),
            visual_adapters: scene.visual_adapters.into(),
        }
    }

    /// The replica after `delta`: `None` when the replica already holds its revision or a newer
    /// one.
    ///
    /// # Errors
    ///
    /// The delta is not a reset and was computed on another revision than the one held.
    pub fn applied(&self, delta: DecorationSceneDelta) -> Result<Option<Self>, SceneGap> {
        if delta.reset {
            if delta.revision < self.revision {
                return Ok(None);
            }
            return Ok(Some(Self {
                revision: delta.revision,
                surfaces: delta
                    .surfaces
                    .into_iter()
                    .map(|(id, surface)| (id, Arc::new(surface)))
                    .collect(),
                animation: delta.animation,
                input_hooks: delta.input_hooks.into(),
                visual_adapters: delta.visual_adapters.into(),
            }));
        }
        if delta.revision <= self.revision {
            return Ok(None);
        }
        if delta.base_revision != self.revision {
            return Err(SceneGap {
                held: self.revision,
                base: delta.base_revision,
            });
        }
        let mut surfaces = self.surfaces.clone();
        for id in &delta.removed {
            surfaces.remove(id);
        }
        for (id, surface) in delta.surfaces {
            surfaces.insert(id, Arc::new(surface));
        }
        Ok(Some(Self {
            revision: delta.revision,
            surfaces,
            animation: delta.animation,
            input_hooks: share_if_equal(&self.input_hooks, delta.input_hooks),
            visual_adapters: share_if_equal(&self.visual_adapters, delta.visual_adapters),
        }))
    }
}

/// `held` again when `next` is the same list, so an unchanged list is not reallocated.
fn share_if_equal<T: PartialEq>(held: &Arc<[T]>, next: Vec<T>) -> Arc<[T]> {
    if **held == *next {
        held.clone()
    } else {
        next.into()
    }
}

/// The replica on `bus`, registering its channel (empty) when there is none.
fn current(bus: &bmux_plugin::EventBus) -> Arc<SceneReplica> {
    if let Ok((current, _)) = bus.subscribe_state::<SceneReplica>(&REPLICA_STATE_KIND) {
        return current;
    }
    let _ = bus.register_state_channel(REPLICA_STATE_KIND, SceneReplica::default());
    Arc::new(SceneReplica::default())
}

/// Relay a whole scene (as read from the decoration view) to the renderers on `bus`, unless they
/// hold a newer one.
pub fn publish_relayed_scene_on(bus: &bmux_plugin::EventBus, scene: DecorationScene) {
    if scene.revision < current(bus).revision {
        return;
    }
    let _ = bus.publish_state(&REPLICA_STATE_KIND, SceneReplica::from_scene(scene));
}

/// Relay a pushed delta to the renderers on `bus`; returns whether the scene changed.
///
/// # Errors
///
/// The delta does not apply on the scene held: the caller reads the whole scene again and
/// relays it with [`publish_relayed_scene_on`].
pub fn relay_scene_delta_on(
    bus: &bmux_plugin::EventBus,
    delta: DecorationSceneDelta,
) -> Result<bool, SceneGap> {
    let Some(next) = current(bus).applied(delta)? else {
        return Ok(false);
    };
    let _ = bus.publish_state(&REPLICA_STATE_KIND, next);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_scene_protocol::delta::scene_delta;
    use bmux_scene_protocol::scene_protocol::Rect;

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
                .collect(),
            animation: None,
            input_hooks: Vec::new(),
            visual_adapters: Vec::new(),
        }
    }

    /// Relayed deltas rebuild every scene the server sent, sharing the surfaces they did not
    /// carry; a delta on another revision is a gap, and a stale one changes nothing.
    #[test]
    fn deltas_rebuild_each_scene_and_share_unchanged_surfaces() {
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let bus = bmux_plugin::EventBus::new();
        let scenes = [
            scene(1, &[(a, 0), (b, 4)]),
            scene(2, &[(a, 0), (b, 5)]),
            scene(3, &[(a, 0), (c, 8)]),
        ];
        let mut sent = None;
        let mut held = Vec::new();
        for next in &scenes {
            let delta = scene_delta(sent.as_ref(), next).expect("a newer scene");
            assert_eq!(relay_scene_delta_on(&bus, delta), Ok(true));
            let replica = current(&bus);
            assert_eq!(*replica, SceneReplica::from_scene(next.clone()));
            held.push(replica);
            sent = Some(next.clone());
        }
        assert!(
            Arc::ptr_eq(&held[0].surfaces[&a], &held[2].surfaces[&a]),
            "a surface no delta carried is the same allocation"
        );

        let stale = scene_delta(Some(&scenes[0]), &scenes[1]).expect("delta");
        assert_eq!(relay_scene_delta_on(&bus, stale), Ok(false));
        let skipped = scene_delta(Some(&scenes[1]), &scene(5, &[(a, 1)])).expect("delta");
        assert_eq!(
            relay_scene_delta_on(&bus, skipped),
            Err(SceneGap { held: 3, base: 2 })
        );
        publish_relayed_scene_on(&bus, scene(5, &[(a, 1)]));
        assert_eq!(current(&bus).revision, 5);
    }

    /// Each view lifetime starts from a whole scene with a newer revision: a later lifetime's
    /// scene replaces the held one, an older lifetime's (queued before a close) cannot revive it,
    /// and a reconnected client starts from nothing.
    #[test]
    fn whole_scenes_fence_old_lifetimes_and_reconnect_starts_fresh() {
        let whole = |revision| scene_delta(None, &scene(revision, &[])).expect("a whole scene");
        let held = SceneReplica::default()
            .applied(whole(40))
            .unwrap()
            .expect("first lifetime");
        assert_eq!(held.applied(whole(39)), Ok(None));
        // Close's empty barrier is newer; queued old-lifetime output cannot revive it.
        let held = held.applied(whole(41)).unwrap().expect("barrier");
        assert_eq!(held.applied(whole(40)), Ok(None));
        let held = held.applied(whole(42)).unwrap().expect("next lifetime");
        assert_eq!(held.revision, 42);
        let reconnected = SceneReplica::default().applied(whole(1)).unwrap();
        assert_eq!(reconnected.map(|replica| replica.revision), Some(1));
    }
}
