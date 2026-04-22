//! Cached view of the decoration plugin's latest [`DecorationScene`].
//!
//! The render path consults this cache once per frame. When a surface
//! is present in the cache the paint commands from the plugin are
//! applied; when the surface is absent, the renderer leaves the pane
//! bare (no compiled-in fallback). The cache is populated via the
//! typed decoration-state service (`scene_snapshot` query) until the
//! push-based scene event stream is fully wired.

use uuid::Uuid;

pub use bmux_scene_protocol::scene_protocol::{
    AnimationHint, BorderGlyphs, Cell, Color, DecorationScene, FallbackStyle, GradientAxis,
    NamedColor, PaintCommand, Rect, Rect as SceneRect, Style, SurfaceDecoration,
};

/// Render-side cache of the latest decoration scene.
///
/// Consumers update the cache by calling [`Self::set_scene`] whenever
/// a fresher scene is observed (initially via a one-shot
/// `scene_snapshot` query; later via a typed event subscription). The
/// cache guards against stale writes by rejecting any scene whose
/// revision is lower than the one currently held.
#[derive(Debug, Clone, Default)]
pub struct DecorationSceneCache {
    scene: Option<DecorationScene>,
}

impl DecorationSceneCache {
    /// Construct an empty cache.
    #[must_use]
    pub const fn new() -> Self {
        Self { scene: None }
    }

    /// Replace the cached scene with `scene` when its revision is
    /// strictly greater than the currently-cached one. Returns `true`
    /// when the cache was updated.
    pub fn set_scene(&mut self, scene: DecorationScene) -> bool {
        match &self.scene {
            Some(existing) if existing.revision >= scene.revision => false,
            _ => {
                self.scene = Some(scene);
                true
            }
        }
    }

    /// Replace the cached scene unconditionally. Intended for tests
    /// and for consumers that manage freshness themselves.
    pub fn force_scene(&mut self, scene: DecorationScene) {
        self.scene = Some(scene);
    }

    /// Clear the cache. The next render frame will see no
    /// decoration data.
    pub fn clear(&mut self) {
        self.scene = None;
    }

    /// Return the revision number of the currently-cached scene, or
    /// `None` if no scene has been cached yet.
    #[must_use]
    pub fn revision(&self) -> Option<u64> {
        self.scene.as_ref().map(|s| s.revision)
    }

    /// Look up the decoration data for a specific surface.
    #[must_use]
    pub fn surface(&self, surface_id: &Uuid) -> Option<&SurfaceDecoration> {
        self.scene
            .as_ref()
            .and_then(|scene| scene.surfaces.get(surface_id))
    }

    /// Return the plugin-published fallback style used for panes that
    /// are not represented explicitly in the scene's `surfaces` map.
    /// Returns `None` when no scene has been cached yet or when the
    /// cached scene carries no fallback.
    #[must_use]
    pub fn fallback_style(&self) -> Option<&FallbackStyle> {
        self.scene
            .as_ref()
            .and_then(|scene| scene.fallback.as_ref())
    }

    /// Whether any scene has been cached yet.
    #[must_use]
    pub const fn has_scene(&self) -> bool {
        self.scene.is_some()
    }

    /// Iterate over every `(surface_id, decoration)` pair in the
    /// cached scene. Yields nothing when the cache is empty.
    pub fn iter(&self) -> impl Iterator<Item = (&Uuid, &SurfaceDecoration)> {
        self.scene.iter().flat_map(|scene| scene.surfaces.iter())
    }
}

/// Shared handle into a [`DecorationSceneCache`]. The attach runtime
/// hands a clone to the render loop for read-only access while its
/// own control thread updates the cache from scene events.
pub type SharedSceneCache = std::sync::Arc<std::sync::RwLock<DecorationSceneCache>>;

/// Construct a shared cache pre-filled with an empty [`DecorationScene`].
#[must_use]
pub fn shared_cache() -> SharedSceneCache {
    std::sync::Arc::new(std::sync::RwLock::new(DecorationSceneCache::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn scene(revision: u64, surface_id: Uuid) -> DecorationScene {
        let mut surfaces = BTreeMap::new();
        surfaces.insert(
            surface_id,
            SurfaceDecoration {
                surface_id,
                rect: SceneRect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                content_rect: SceneRect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                paint_commands: Vec::new(),
            },
        );
        DecorationScene {
            revision,
            surfaces,
            fallback: None,
            animation: None,
        }
    }

    #[test]
    fn empty_cache_has_no_scene() {
        let cache = DecorationSceneCache::new();
        assert!(!cache.has_scene());
        assert!(cache.revision().is_none());
    }

    #[test]
    fn set_scene_accepts_newer_revision() {
        let mut cache = DecorationSceneCache::new();
        let id = Uuid::from_u128(1);
        assert!(cache.set_scene(scene(1, id)));
        assert_eq!(cache.revision(), Some(1));
        assert!(cache.set_scene(scene(2, id)));
        assert_eq!(cache.revision(), Some(2));
    }

    #[test]
    fn set_scene_rejects_older_or_equal_revision() {
        let mut cache = DecorationSceneCache::new();
        let id = Uuid::from_u128(1);
        cache.force_scene(scene(5, id));
        assert!(!cache.set_scene(scene(5, id)));
        assert!(!cache.set_scene(scene(4, id)));
        assert_eq!(cache.revision(), Some(5));
    }

    #[test]
    fn surface_lookup_returns_entry_for_known_surface() {
        let mut cache = DecorationSceneCache::new();
        let id = Uuid::from_u128(42);
        cache.set_scene(scene(1, id));
        assert!(cache.surface(&id).is_some());
        assert!(cache.surface(&Uuid::from_u128(99)).is_none());
    }

    #[test]
    fn clear_drops_scene() {
        let mut cache = DecorationSceneCache::new();
        let id = Uuid::from_u128(1);
        cache.set_scene(scene(7, id));
        cache.clear();
        assert!(!cache.has_scene());
    }
}
