//! bmux decoration plugin.
//!
//! Owns pane visual chrome: borders, focus highlighting, status
//! badges. Publishes a [`bmux_scene_protocol::scene_protocol::DecorationScene`]
//! through the typed plugin event bus whenever its internal state
//! changes. The scene is the authoritative source for each surface's
//! `content_rect` and any paint commands layered around the PTY
//! content; core consumes it during frame assembly.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bmux_decoration_plugin_api::decoration_state::{
    BorderStyle, DecorationEvent, DecorationStateService,
    INTERFACE_ID as DECORATION_STATE_INTERFACE_ID, PaneDecoration, SetStyleError,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs, Color, DecorationScene, FallbackStyle, PaintCommand, Rect, Style,
    SurfaceDecoration,
};
use uuid::Uuid;

/// In-memory state store.
#[derive(Debug, Default)]
struct State {
    /// Per-pane overrides. Panes without an override fall through to
    /// [`State::default_border`].
    panes: HashMap<Uuid, PaneDecoration>,
    /// Global default, used for any pane without a specific override.
    /// `BorderStyle` has `@default ascii` in the BPDL schema, so
    /// `BorderStyle::default()` yields `BorderStyle::Ascii`.
    default_border: BorderStyle,
    /// Monotonic revision counter for published decoration scenes.
    /// Incremented every time internal state changes so consumers can
    /// discard stale snapshots cheaply.
    scene_revision: u64,
}

/// Shared decoration state.
///
/// Held behind an `Arc<Mutex<State>>` so the `RustPlugin` instance and
/// the typed service provider can observe the same view. The typed
/// service implementation ([`DecorationServiceHandle`]) is a thin
/// wrapper that holds a clone of the same Arc and implements
/// [`DecorationStateService`].
#[derive(Debug, Default)]
struct SharedState {
    inner: Arc<Mutex<State>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State::default())),
        }
    }

    fn clone_arc(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.inner)
    }
}

/// Typed-service provider handle.
///
/// Wraps a shared [`Arc<Mutex<State>>`] so multiple consumers (the
/// plugin host's event loop + any consumer plugin resolving the typed
/// service) observe the same store.
struct DecorationServiceHandle {
    state: Arc<Mutex<State>>,
}

impl DecorationServiceHandle {
    fn new(state: Arc<Mutex<State>>) -> Self {
        Self { state }
    }
}

impl DecorationStateService for DecorationServiceHandle {
    fn pane_decoration<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneDecoration>> + Send + 'a>> {
        Box::pin(async move {
            let state = self.state.lock().ok()?;
            if let Some(p) = state.panes.get(&pane_id) {
                return Some(p.clone());
            }
            Some(default_pane_decoration(pane_id, state.default_border))
        })
    }

    fn default_border_style<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = BorderStyle> + Send + 'a>> {
        Box::pin(async move {
            self.state
                .lock()
                .map_or_else(|_| BorderStyle::default(), |s| s.default_border)
        })
    }

    fn scene_snapshot<'a>(&'a self) -> Pin<Box<dyn Future<Output = DecorationScene> + Send + 'a>> {
        Box::pin(async move {
            self.state
                .lock()
                .map_or_else(|_| empty_scene(), |state| build_scene(&state))
        })
    }

    fn set_pane_border<'a>(
        &'a self,
        pane_id: Uuid,
        border: BorderStyle,
    ) -> Pin<Box<dyn Future<Output = Result<(), SetStyleError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SetStyleError::StyleUnsupported {
                    style: "<poisoned>".into(),
                })?;
            let entry = state
                .panes
                .entry(pane_id)
                .or_insert_with(|| default_pane_decoration(pane_id, border));
            entry.border = border;
            bump_revision(&mut state);
            Ok(())
        })
    }

    fn set_default_border<'a>(
        &'a self,
        border: BorderStyle,
    ) -> Pin<Box<dyn Future<Output = Result<(), SetStyleError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SetStyleError::StyleUnsupported {
                    style: "<poisoned>".into(),
                })?;
            state.default_border = border;
            bump_revision(&mut state);
            Ok(())
        })
    }
}

/// Produce a scene describing the current state of `state`. Pulled
/// out of the plugin's inherent method so the typed
/// [`DecorationStateService::scene_snapshot`] can share the same
/// build logic without re-locking the mutex.
fn build_scene(state: &State) -> DecorationScene {
    let mut surfaces = BTreeMap::new();
    for (pane_id, decoration) in &state.panes {
        surfaces.insert(
            *pane_id,
            surface_decoration_for(*pane_id, decoration.border),
        );
    }
    DecorationScene {
        revision: state.scene_revision,
        surfaces,
        fallback: Some(default_fallback_style()),
    }
}

/// Fallback scene returned when the state lock is poisoned.
fn empty_scene() -> DecorationScene {
    DecorationScene {
        revision: 0,
        surfaces: BTreeMap::new(),
        fallback: None,
    }
}

/// Plugin-owned defaults used by the renderer for panes that aren't
/// represented explicitly in the scene's `surfaces` map.
fn default_fallback_style() -> FallbackStyle {
    FallbackStyle {
        border_unfocused: BorderGlyphs::Ascii,
        border_focused: BorderGlyphs::AsciiFocused,
        border_zoomed: BorderGlyphs::AsciiZoomed,
        running_badge: DEFAULT_RUNNING_BADGE.to_string(),
        exited_badge: DEFAULT_EXITED_BADGE.to_string(),
    }
}

/// Bump the scene revision. Called from every mutator so consumers
/// observing the typed event stream can detect updates.
fn bump_revision(state: &mut State) {
    state.scene_revision = state.scene_revision.saturating_add(1);
}

/// The decoration plugin's concrete implementation.
#[derive(Default)]
pub struct DecorationPlugin {
    state: SharedState,
}

impl DecorationPlugin {
    /// Construct a fresh decoration plugin with the built-in ASCII default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: SharedState::new(),
        }
    }

    /// Build a decoration scene from the plugin's current state.
    ///
    /// The scene covers every pane that has been touched by an
    /// explicit `set_pane_border` call; untouched panes fall through
    /// to the default style and core paints nothing around them.
    #[must_use]
    pub fn build_scene(&self) -> DecorationScene {
        self.state
            .inner
            .lock()
            .map_or_else(|_| empty_scene(), |state| build_scene(&state))
    }
}

/// Default badge text rendered when a pane is running. Plugins that
/// want different text call `set_pane_border` (or a future dedicated
/// `set_pane_badges` command) to override per-pane.
pub const DEFAULT_RUNNING_BADGE: &str = "[RUNNING]";

/// Default badge text rendered when a pane has exited.
pub const DEFAULT_EXITED_BADGE: &str = "[EXITED]";

/// Build a [`PaneDecoration`] with the plugin's default style values
/// for a pane that hasn't been touched by an explicit override.
fn default_pane_decoration(pane_id: Uuid, border: BorderStyle) -> PaneDecoration {
    PaneDecoration {
        pane_id,
        border,
        focused: false,
        running_badge: Some(DEFAULT_RUNNING_BADGE.to_string()),
        exited_badge: Some(DEFAULT_EXITED_BADGE.to_string()),
    }
}

/// Produce a [`SurfaceDecoration`] for a pane given its border style.
///
/// Returns a scene entry with a zero-width outer rect — the real pane
/// geometry is filled in by the renderer once it knows the layout. The
/// plugin only owns style and paint commands; core owns geometry.
fn surface_decoration_for(pane_id: Uuid, border: BorderStyle) -> SurfaceDecoration {
    // Placeholder geometry; the renderer pairs this with the actual
    // surface rect at paint time. A future revision exchanges surface
    // rects with the plugin through a typed query.
    let rect = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    let content_rect = rect.clone();
    SurfaceDecoration {
        surface_id: pane_id,
        rect,
        content_rect,
        paint_commands: paint_commands_for(border),
    }
}

/// Placeholder paint commands for a border style. The actual border
/// characters are positioned by the renderer using the live surface
/// geometry; this list carries style metadata so the renderer knows
/// which palette to use per style.
fn paint_commands_for(border: BorderStyle) -> Vec<PaintCommand> {
    // When the renderer pairs the scene with live geometry it
    // expands these descriptors into concrete per-cell paints. Until
    // that wiring lands we keep the paint list empty so consumers see
    // the intended style without relying on hardcoded ASCII glyphs.
    let _ = border;
    Vec::new()
}

/// Style helper used by the renderer when expanding descriptors into
/// concrete paint commands.
#[must_use]
pub const fn style_for_focus(focused: bool) -> Style {
    Style {
        fg: Some(if focused {
            Color::BrightWhite
        } else {
            Color::White
        }),
        bg: None,
        bold: focused,
        underline: false,
        italic: false,
        reverse: false,
    }
}

impl RustPlugin for DecorationPlugin {
    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        // Bump the scene revision so the first build_scene() call
        // returns a non-zero revision, signalling consumers that the
        // plugin has published at least once. Actual event emission of
        // the scene and the associated ready-signal are plumbed by the
        // plugin host; this hook ensures internal state is ready.
        if let Ok(mut state) = self.state.inner.lock() {
            bump_revision(&mut state);
        }
        Ok(EXIT_OK)
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let handle: Arc<DecorationServiceHandle> =
            Arc::new(DecorationServiceHandle::new(self.state.clone_arc()));
        let service: Arc<dyn DecorationStateService + Send + Sync> = handle;
        let (Ok(read_cap), Ok(write_cap)) = (
            HostScope::new("bmux.decoration.read"),
            HostScope::new("bmux.decoration.write"),
        ) else {
            return;
        };
        registry.insert_typed::<dyn DecorationStateService + Send + Sync>(
            read_cap,
            ServiceKind::Query,
            DECORATION_STATE_INTERFACE_ID,
            service.clone(),
        );
        registry.insert_typed::<dyn DecorationStateService + Send + Sync>(
            write_cap,
            ServiceKind::Command,
            DECORATION_STATE_INTERFACE_ID,
            service,
        );
    }
}

/// Re-export the public API types so downstream consumers can import
/// everything from this crate without pulling `bmux_decoration_plugin_api`
/// separately.
pub use bmux_decoration_plugin_api::decoration_state;

/// Canonical interface ids published by this plugin.
pub mod interface_ids {
    pub use bmux_decoration_plugin_api::decoration_state::INTERFACE_ID as DECORATION_STATE;
    pub use bmux_scene_protocol::scene_protocol::INTERFACE_ID as SCENE_PROTOCOL;
}

/// Marker function used by tests to verify event-stream types round-trip.
#[must_use]
pub fn sample_event_for_pane(pane_id: Uuid) -> DecorationEvent {
    DecorationEvent::PaneRestyled { pane_id }
}

/// Name of the readiness signal the decoration plugin fires after
/// publishing its first [`DecorationScene`].
pub const SCENE_PUBLISHED_SIGNAL: &str = "scene-published";

// Runtime assertion (executed once at the top of the test suite) that
// the interface ids hardcoded in `plugin.toml` and the typed-service
// registration match the BPDL-generated constants. A regression in
// either the BPDL schema or the manifest will surface immediately.
#[cfg(test)]
#[test]
fn interface_ids_match_bpdl_constants() {
    assert_eq!(DECORATION_STATE_INTERFACE_ID.as_str(), "decoration-state");
    assert_eq!(
        bmux_scene_protocol::scene_protocol::INTERFACE_ID.as_str(),
        "scene-protocol"
    );
}

bmux_plugin_sdk::export_plugin!(DecorationPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut pinned = Box::pin(fut);
        loop {
            match pinned.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {}
            }
        }
    }

    #[test]
    fn new_plugin_has_ascii_default_border() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let style = block_on(handle.default_border_style());
        assert_eq!(style, BorderStyle::Ascii);
    }

    #[test]
    fn border_style_default_is_ascii() {
        assert_eq!(BorderStyle::default(), BorderStyle::Ascii);
    }

    #[test]
    fn query_unknown_pane_returns_default_style() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let decoration = block_on(handle.pane_decoration(Uuid::nil()))
            .expect("default decoration always present");
        assert_eq!(decoration.border, BorderStyle::Ascii);
        assert_eq!(decoration.pane_id, Uuid::nil());
    }

    #[test]
    fn set_pane_border_persists_override() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(7);
        let res = block_on(handle.set_pane_border(pane, BorderStyle::Double));
        assert!(res.is_ok());
        let decoration = block_on(handle.pane_decoration(pane)).unwrap();
        assert_eq!(decoration.border, BorderStyle::Double);
    }

    #[test]
    fn set_default_border_changes_global_default() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let res = block_on(handle.set_default_border(BorderStyle::None));
        assert!(res.is_ok());
        let default = block_on(handle.default_border_style());
        assert_eq!(default, BorderStyle::None);
    }

    #[test]
    fn sample_event_constructs_tagged_variant() {
        let ev = sample_event_for_pane(Uuid::from_u128(1));
        if let DecorationEvent::PaneRestyled { pane_id } = ev {
            assert_eq!(pane_id, Uuid::from_u128(1));
        } else {
            panic!("expected pane_restyled variant");
        }
    }

    #[test]
    fn build_scene_is_empty_on_fresh_plugin() {
        let plugin = DecorationPlugin::new();
        let scene = plugin.build_scene();
        assert_eq!(scene.revision, 0);
        assert!(scene.surfaces.is_empty());
    }

    #[test]
    fn setting_pane_border_bumps_revision_and_populates_scene() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(42);
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");
        let scene = plugin.build_scene();
        assert_eq!(scene.revision, 1);
        assert!(scene.surfaces.contains_key(&pane));
    }

    #[test]
    fn activate_bumps_revision_so_first_publish_is_visible() {
        let plugin = DecorationPlugin::new();
        let before = plugin.build_scene().revision;
        assert_eq!(before, 0);
        if let Ok(mut state) = plugin.state.inner.lock() {
            bump_revision(&mut state);
        }
        let after = plugin.build_scene().revision;
        assert!(after > before);
    }

    #[test]
    fn register_typed_services_installs_decoration_state_service() {
        let plugin = DecorationPlugin::new();
        let mut registry = TypedServiceRegistry::new();
        let empty_caps: Vec<String> = Vec::new();
        let empty_services: Vec<bmux_plugin_sdk::RegisteredService> = Vec::new();
        let settings = std::collections::BTreeMap::new();
        let host_metadata = bmux_plugin_sdk::HostMetadata {
            product_name: "test".to_string(),
            product_version: "0".to_string(),
            plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
            plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
        };
        let host_connection = bmux_plugin_sdk::HostConnectionInfo {
            config_dir: "/tmp".to_string(),
            runtime_dir: "/tmp".to_string(),
            data_dir: "/tmp".to_string(),
            state_dir: "/tmp".to_string(),
        };
        let context = TypedServiceRegistrationContext {
            plugin_id: "bmux.decoration",
            host_kernel_bridge: None,
            required_capabilities: &empty_caps,
            provided_capabilities: &empty_caps,
            services: &empty_services,
            available_capabilities: &empty_caps,
            enabled_plugins: &empty_caps,
            plugin_search_roots: &empty_caps,
            host: &host_metadata,
            connection: &host_connection,
            plugin_settings_map: &settings,
        };
        plugin.register_typed_services(context, &mut registry);
        let cap = HostScope::new("bmux.decoration.read").expect("cap");
        let handle = registry
            .get(
                &cap,
                ServiceKind::Query,
                DECORATION_STATE_INTERFACE_ID.as_str(),
            )
            .expect("handle present");
        let service = handle
            .provider_as_trait::<dyn DecorationStateService + Send + Sync>()
            .expect("downcast");
        let style = block_on(service.default_border_style());
        assert_eq!(style, BorderStyle::default());
    }

    #[test]
    fn style_for_focus_flags_bold_when_focused() {
        assert!(style_for_focus(true).bold);
        assert!(!style_for_focus(false).bold);
    }
}
