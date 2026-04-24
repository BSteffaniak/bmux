//! Plugin-supplied attach render extensions.
//!
//! A render extension is a plugin-side object that the attach runtime
//! queries during frame assembly to paint per-surface chrome on top
//! of pane content. It is the generic hook that replaces historical
//! decoration-specific bridging paths: any plugin that wants to draw
//! borders, overlays, badges, or other surface decoration registers
//! an `AttachRenderExtension` impl and is consulted once per visible
//! surface on every render pass.
//!
//! # Lifecycle
//!
//! 1. During the plugin's activation (or later, whenever the plugin
//!    decides it has something to render), it calls
//!    [`register_render_extension`] with an `Arc<dyn AttachRenderExtension>`.
//! 2. The attach runtime reads the current registry via
//!    [`registered_render_extensions`] on every frame and calls each
//!    extension's [`AttachRenderExtension::apply_surface`] for every
//!    visible surface.
//! 3. When a surface disappears (pane closed, layout recomputed without
//!    it), the attach runtime calls
//!    [`AttachRenderExtension::surface_removed`] so the extension can
//!    evict any cached state.
//!
//! Extensions are expected to be lightweight: the registry lookup is
//! `O(n)` per render, and `apply_surface` is on the hot path. Caching
//! paint output on the extension side is recommended when the source
//! data (e.g. a scene-protocol snapshot) changes less often than
//! layout refreshes.

use std::io;
use std::sync::{Arc, OnceLock, RwLock};
use uuid::Uuid;

/// Minimal rectangle used by render extensions to report content-rect
/// adjustments back to the attach runtime.
///
/// This is structurally identical to the scene-protocol `Rect` but is
/// defined here to keep the extension trait free of a
/// scene-protocol dependency: generic extensions (e.g. a future
/// overlay plugin that doesn't produce scene-protocol output) can
/// still speak the trait without importing wire-schema types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Host-supplied trait objects that plugins implement to paint
/// per-surface chrome on top of pane content.
///
/// All methods are called on the attach-runtime thread. Implementations
/// must be `Send + Sync` because the registry stores them behind
/// `Arc<dyn AttachRenderExtension>`.
pub trait AttachRenderExtension: Send + Sync {
    /// Human-readable extension name. Used for tracing and error
    /// messages. Convention: `<plugin-id>.<extension-kind>` (e.g.
    /// `"bmux.decoration.renderer"`).
    fn name(&self) -> &str;

    /// Paint any per-surface chrome onto `stdout` for the surface at
    /// `surface_rect`. Returns `Ok(true)` when any bytes were written
    /// (so the caller can terminate a styled run with a reset), or
    /// `Ok(false)` when the extension had nothing to paint for this
    /// surface.
    ///
    /// # Errors
    ///
    /// Returns any error from queueing bytes onto `stdout`.
    fn apply_surface(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
    ) -> io::Result<bool>;

    /// Override the surface's content-rect inset. Returning `Some`
    /// tells the attach runtime "the PTY should render inside this
    /// smaller rectangle"; `None` means the extension has no opinion.
    /// When multiple extensions return `Some`, the attach runtime
    /// picks the narrowest inset.
    fn content_rect_override(&self, _surface_id: Uuid) -> Option<ExtensionRect> {
        None
    }

    /// Called when a surface is removed from the attach layout. The
    /// extension should evict any cached state for `surface_id`.
    fn surface_removed(&self, _surface_id: Uuid) {}
}

/// Thread-safe registry of render extensions.
///
/// Extensions are typically registered once during plugin activation
/// and persist for the lifetime of the process. Registration order
/// determines extension invocation order during rendering; callers
/// should not rely on it for correctness.
#[derive(Default)]
pub struct RenderExtensionRegistry {
    entries: RwLock<Vec<Arc<dyn AttachRenderExtension>>>,
}

impl std::fmt::Debug for RenderExtensionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.read().map_or(0, |g| g.len());
        f.debug_struct("RenderExtensionRegistry")
            .field("entries", &count)
            .finish()
    }
}

impl RenderExtensionRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an extension. Extensions are consulted in registration
    /// order during rendering.
    pub fn register(&self, ext: Arc<dyn AttachRenderExtension>) {
        if let Ok(mut guard) = self.entries.write() {
            guard.push(ext);
        }
    }

    /// Snapshot of currently-registered extensions. Callers iterate
    /// the returned `Vec` on their own thread; extension invocation
    /// does not hold the registry lock.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Arc<dyn AttachRenderExtension>> {
        self.entries.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Number of registered extensions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().map_or(0, |g| g.len())
    }

    /// `true` when no extension is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-wide shared extension registry.
///
/// Used by plugins whose activation callback does not carry a
/// registry handle in its context. Core code reading extensions and
/// plugins registering them both go through this singleton.
#[must_use]
pub fn global_render_extension_registry() -> Arc<RenderExtensionRegistry> {
    static GLOBAL: OnceLock<Arc<RenderExtensionRegistry>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(RenderExtensionRegistry::new()))
        .clone()
}

/// Register a render extension on the process-wide registry. Shortcut
/// for `global_render_extension_registry().register(ext)`.
pub fn register_render_extension(ext: Arc<dyn AttachRenderExtension>) {
    global_render_extension_registry().register(ext);
}

/// Snapshot of currently-registered render extensions. Shortcut for
/// `global_render_extension_registry().snapshot()`.
#[must_use]
pub fn registered_render_extensions() -> Vec<Arc<dyn AttachRenderExtension>> {
    global_render_extension_registry().snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingExtension {
        name: String,
        applied: Mutex<Vec<Uuid>>,
        removed: Mutex<Vec<Uuid>>,
    }

    impl AttachRenderExtension for RecordingExtension {
        fn name(&self) -> &str {
            &self.name
        }

        fn apply_surface(
            &self,
            _stdout: &mut dyn io::Write,
            surface_id: Uuid,
            _surface_rect: &ExtensionRect,
        ) -> io::Result<bool> {
            self.applied.lock().unwrap().push(surface_id);
            Ok(false)
        }

        fn surface_removed(&self, surface_id: Uuid) {
            self.removed.lock().unwrap().push(surface_id);
        }
    }

    #[test]
    fn registry_tracks_registration_order() {
        let registry = RenderExtensionRegistry::new();
        let a = Arc::new(RecordingExtension {
            name: "a".to_string(),
            applied: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
        }) as Arc<dyn AttachRenderExtension>;
        let b = Arc::new(RecordingExtension {
            name: "b".to_string(),
            applied: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
        }) as Arc<dyn AttachRenderExtension>;
        registry.register(a);
        registry.register(b);
        assert_eq!(registry.len(), 2);
        let snap = registry.snapshot();
        assert_eq!(snap[0].name(), "a");
        assert_eq!(snap[1].name(), "b");
    }

    #[test]
    fn empty_registry_is_empty() {
        let registry = RenderExtensionRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn extension_default_content_rect_override_is_none() {
        let ext = RecordingExtension {
            name: "x".to_string(),
            applied: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
        };
        assert!(ext.content_rect_override(Uuid::nil()).is_none());
    }
}
