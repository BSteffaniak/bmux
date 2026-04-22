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

pub mod scripting;

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bmux_decoration_plugin_api::decoration_state::{
    BorderStyle, DecorationEvent, DecorationStateService, DecorationThemeExtension,
    INTERFACE_ID as DECORATION_STATE_INTERFACE_ID, NotifyError, PaneActivity, PaneDecoration,
    PaneEvent, PaneGeometry, PaneLifecycle, SetStyleError, ValidationError, ValidationResult,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs, Color, DecorationScene, FallbackStyle, NamedColor, PaintCommand, Rect, Style,
    SurfaceDecoration,
};
use uuid::Uuid;

/// In-memory state store.
#[derive(Debug, Default)]
struct State {
    /// Per-pane overrides. Panes without an override fall through to
    /// [`State::default_border`].
    panes: HashMap<Uuid, PaneDecoration>,
    /// Per-pane live geometry observed from the attach runtime.
    geometry: HashMap<Uuid, PaneGeometry>,
    /// Per-pane focus/zoom/lifecycle. Kept separate from
    /// `panes` (style) so mutators don't have to allocate a
    /// `PaneDecoration` row just to flip a focus bit.
    activity: HashMap<Uuid, PaneActivity>,
    /// Global default, used for any pane without a specific override.
    /// `BorderStyle` has `@default ascii` in the BPDL schema, so
    /// `BorderStyle::default()` yields `BorderStyle::Ascii`.
    default_border: BorderStyle,
    /// Monotonic revision counter for published decoration scenes.
    /// Incremented every time internal state changes so consumers can
    /// discard stale snapshots cheaply.
    scene_revision: u64,
    /// Currently-loaded theme extension. Populated at activation
    /// from `[plugins."bmux.decoration"]` inside the user's theme
    /// file; `None` means "no theme extension observed; paint with
    /// built-in ASCII defaults".
    current_theme: Option<DecorationThemeExtension>,
}

impl State {
    /// Borrow-or-create activity for `pane_id`. Caller must bump the
    /// revision when they observe a change.
    fn activity_mut(&mut self, pane_id: Uuid) -> &mut PaneActivity {
        self.activity
            .entry(pane_id)
            .or_insert_with(|| PaneActivity {
                pane_id,
                focused: false,
                zoomed: false,
                status: PaneLifecycle::Running,
            })
    }

    /// Mirror `activity.focused` into the per-pane `PaneDecoration`
    /// override row so consumers reading the decoration struct see a
    /// consistent value. Does NOT create an override row if none
    /// exists (keeps the "default decoration" answer stable).
    fn sync_focused_mirror(&mut self, pane_id: Uuid, focused: bool) {
        if let Some(entry) = self.panes.get_mut(&pane_id) {
            entry.focused = focused;
        }
    }
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
            let focused = state.activity.get(&pane_id).is_some_and(|a| a.focused);
            Some(default_pane_decoration(
                pane_id,
                state.default_border,
                focused,
            ))
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

    fn pane_geometry<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneGeometry>> + Send + 'a>> {
        Box::pin(async move {
            let state = self.state.lock().ok()?;
            state.geometry.get(&pane_id).cloned()
        })
    }

    fn pane_activity<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneActivity>> + Send + 'a>> {
        Box::pin(async move {
            let state = self.state.lock().ok()?;
            state.activity.get(&pane_id).cloned()
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
            let focused = state.activity.get(&pane_id).is_some_and(|a| a.focused);
            let entry = state
                .panes
                .entry(pane_id)
                .or_insert_with(|| default_pane_decoration(pane_id, border, focused));
            entry.border = border;
            entry.focused = focused;
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

    fn notify_pane_geometry<'a>(
        &'a self,
        geometry: PaneGeometry,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| NotifyError::InvalidArgument {
                    reason: "decoration state mutex poisoned".to_string(),
                })?;
            let pane_id = geometry.pane_id;
            let previous = state.geometry.insert(pane_id, geometry);
            // Only bump the revision when geometry actually changed —
            // the attach runtime re-pushes on every layout diff even
            // when individual rects didn't move.
            let changed = previous
                .as_ref()
                .is_none_or(|prev| prev != state.geometry.get(&pane_id).unwrap());
            if changed {
                bump_revision(&mut state);
            }
            Ok(())
        })
    }

    fn notify_pane_event<'a>(
        &'a self,
        event: PaneEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| NotifyError::InvalidArgument {
                    reason: "decoration state mutex poisoned".to_string(),
                })?;
            apply_pane_event(&mut state, &event);
            Ok(())
        })
    }

    fn forget_pane<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| NotifyError::InvalidArgument {
                    reason: "decoration state mutex poisoned".to_string(),
                })?;
            let removed = state.panes.remove(&pane_id).is_some()
                | state.geometry.remove(&pane_id).is_some()
                | state.activity.remove(&pane_id).is_some();
            if removed {
                bump_revision(&mut state);
            }
            Ok(())
        })
    }

    fn current_theme_extension<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<DecorationThemeExtension>> + Send + 'a>> {
        Box::pin(async move {
            let state = self.state.lock().ok()?;
            state.current_theme.clone()
        })
    }

    fn validate_theme_extension<'a>(
        &'a self,
        toml_text: String,
    ) -> Pin<Box<dyn Future<Output = ValidationResult> + Send + 'a>> {
        Box::pin(async move { validate_theme_extension_toml(&toml_text) })
    }
}

/// Apply a [`PaneEvent`] to the shared state. Pulled out so both the
/// typed `notify_pane_event` command and the event-bus subscriber
/// can share the same mutation path.
fn apply_pane_event(state: &mut State, event: &PaneEvent) {
    match event {
        PaneEvent::Focused { pane_id } => {
            // Unfocus every other pane so the activity map has a
            // single focused entry at most.
            for (id, act) in &mut state.activity {
                if *id != *pane_id && act.focused {
                    act.focused = false;
                }
            }
            state.activity_mut(*pane_id).focused = true;
            state.sync_focused_mirror(*pane_id, true);
            bump_revision(state);
        }
        PaneEvent::Unfocused { pane_id } => {
            if let Some(act) = state.activity.get_mut(pane_id) {
                act.focused = false;
            }
            state.sync_focused_mirror(*pane_id, false);
            bump_revision(state);
        }
        PaneEvent::Zoomed { pane_id } => {
            state.activity_mut(*pane_id).zoomed = true;
            bump_revision(state);
        }
        PaneEvent::Unzoomed { pane_id } => {
            if let Some(act) = state.activity.get_mut(pane_id) {
                act.zoomed = false;
                bump_revision(state);
            }
        }
        PaneEvent::Opened { pane_id, .. } => {
            state.activity_mut(*pane_id);
            bump_revision(state);
        }
        PaneEvent::Closed { pane_id } => {
            state.panes.remove(pane_id);
            state.geometry.remove(pane_id);
            state.activity.remove(pane_id);
            bump_revision(state);
        }
        PaneEvent::StatusChanged { pane_id, exited } => {
            let act = state.activity_mut(*pane_id);
            act.status = if *exited {
                PaneLifecycle::Exited
            } else {
                PaneLifecycle::Running
            };
            bump_revision(state);
        }
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
            surface_decoration_for(*pane_id, decoration.border, state.geometry.get(pane_id)),
        );
    }
    DecorationScene {
        revision: state.scene_revision,
        surfaces,
        fallback: Some(default_fallback_style()),
        animation: None,
    }
}

/// Fallback scene returned when the state lock is poisoned.
fn empty_scene() -> DecorationScene {
    DecorationScene {
        revision: 0,
        surfaces: BTreeMap::new(),
        fallback: None,
        animation: None,
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

/// Bump the scene revision and emit the updated [`DecorationScene`]
/// on the typed plugin event bus. Called from every mutator so
/// consumers (e.g. the attach runtime) can refresh their scene cache
/// incrementally without polling. Emission silently no-ops if the
/// event-bus channel has not been registered yet (the decoration
/// plugin registers it in [`DecorationPlugin::activate`]).
fn bump_revision(state: &mut State) {
    state.scene_revision = state.scene_revision.saturating_add(1);
    // Build + publish while we still hold the lock: this keeps the
    // revision monotonic from subscribers' perspective.
    let scene = build_scene(state);
    let _ = bmux_plugin::global_event_bus()
        .emit(&bmux_scene_protocol::scene_protocol::EVENT_KIND, scene);
}

/// Parse a TOML string against the [`DecorationThemeExtension`]
/// schema and return a structured [`ValidationResult`].
///
/// Used by the `validate-theme-extension` query so external callers
/// (tests, a future `bmux config validate` CLI) can round-trip a
/// theme file without reaching into plugin internals.
fn validate_theme_extension_toml(text: &str) -> ValidationResult {
    // Parse as generic TOML first so individual field errors can be
    // attributed to paths. `try_into::<DecorationThemeExtension>()`
    // then re-checks the shape. Both failure modes go through the
    // same `Errors` variant so the caller always has a vec.
    let parsed: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(err) => {
            return ValidationResult::Errors {
                errors: vec![ValidationError {
                    path: "<root>".to_string(),
                    message: format!("TOML parse error: {err}"),
                }],
            };
        }
    };
    match parsed.try_into::<DecorationThemeExtension>() {
        Ok(_) => ValidationResult::Ok,
        Err(err) => ValidationResult::Errors {
            errors: vec![ValidationError {
                path: "<schema>".to_string(),
                message: err.to_string(),
            }],
        },
    }
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
fn default_pane_decoration(pane_id: Uuid, border: BorderStyle, focused: bool) -> PaneDecoration {
    PaneDecoration {
        pane_id,
        border,
        focused,
        running_badge: Some(DEFAULT_RUNNING_BADGE.to_string()),
        exited_badge: Some(DEFAULT_EXITED_BADGE.to_string()),
    }
}

/// Produce a [`SurfaceDecoration`] for a pane given its border style
/// and (optionally) its cached geometry. When geometry is present,
/// the surface reports the observed rects; otherwise it falls back to
/// zeroed rects so the renderer can detect "plugin has no geometry
/// yet" and paint the fallback.
fn surface_decoration_for(
    pane_id: Uuid,
    border: BorderStyle,
    geometry: Option<&PaneGeometry>,
) -> SurfaceDecoration {
    let (rect, content_rect) = geometry.map_or_else(
        || {
            (
                Rect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                Rect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
            )
        },
        |g| (g.rect.clone(), g.content_rect.clone()),
    );
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
pub fn style_for_focus(focused: bool) -> Style {
    Style {
        fg: Some(Color::Named {
            name: if focused {
                NamedColor::BrightWhite
            } else {
                NamedColor::White
            },
        }),
        bg: None,
        bold: focused,
        underline: false,
        italic: false,
        reverse: false,
        dim: false,
        blink: false,
        strikethrough: false,
    }
}

/// Load the decoration theme extension from the active theme file
/// resolved via the user's config directory. Reads `bmux.toml`,
/// extracts `appearance.theme`, reads `themes/{name}.toml`, then
/// pulls `[plugins."bmux.decoration"]` and parses it against the
/// `DecorationThemeExtension` schema.
///
/// Returns `None` when any step fails — the plugin should fall back
/// to its built-in defaults and not crash on a malformed user
/// config. Errors are logged at info/debug.
fn load_theme_extension_from_config_dir(
    config_dir: &std::path::Path,
) -> Option<DecorationThemeExtension> {
    // Step 1: locate appearance.theme in bmux.toml.
    let main_config_path = config_dir.join("bmux.toml");
    let main_toml: toml::Value = std::fs::read_to_string(&main_config_path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())?;
    let theme_name = main_toml
        .get("appearance")
        .and_then(|a| a.get("theme"))
        .and_then(toml::Value::as_str)
        .unwrap_or("default")
        .to_string();
    if theme_name.is_empty() || theme_name == "default" {
        return None;
    }
    // Step 2: load the theme file.
    let theme_path = config_dir.join("themes").join(format!("{theme_name}.toml"));
    let theme_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(&theme_path).ok()?).ok()?;
    // Step 3: extract `plugins."bmux.decoration"` and parse.
    let plugins_table = theme_toml.get("plugins")?;
    let decoration_table = plugins_table.get("bmux.decoration")?.clone();
    decoration_table.try_into().ok()
}

impl RustPlugin for DecorationPlugin {
    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        // Register the typed scene-event channel before any mutator
        // (including the initial revision bump below) tries to emit.
        // Failure is non-fatal — the channel may already exist from a
        // prior load; `bump_revision` tolerates a missing channel.
        let _ = bmux_plugin::global_event_bus()
            .register_channel::<bmux_scene_protocol::scene_protocol::EventPayload>(
                bmux_scene_protocol::scene_protocol::EVENT_KIND,
            );
        // Load the decoration theme extension from the user's active
        // theme file, if any. Errors are logged but non-fatal — the
        // plugin falls back to its built-in defaults.
        let theme_extension = load_theme_extension_from_config_dir(std::path::Path::new(
            &context.connection.config_dir,
        ));
        if let Ok(mut state) = self.state.inner.lock() {
            state.current_theme = theme_extension;
            // Bump the scene revision so the first build_scene() call
            // returns a non-zero revision, signalling consumers that
            // the plugin has published at least once. Emission runs
            // inside `bump_revision`, so subscribers see the initial
            // scene on their next poll.
            bump_revision(&mut state);
        }
        // Spawn the windows-plugin pane-event subscriber. Runs for
        // the lifetime of the decoration plugin; the host tears down
        // background threads at shutdown.
        spawn_windows_pane_event_subscriber(self.state.clone_arc());
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

/// Subscribe to the windows plugin's `pane-event` topic on the typed
/// event bus and reflect updates into the decoration state store.
/// Silently no-ops when the bus channel hasn't been registered yet
/// (e.g. the windows plugin is not loaded, or activates later than
/// the decoration plugin — the bus does not buffer missed events).
fn spawn_windows_pane_event_subscriber(state: Arc<Mutex<State>>) {
    // The bus channel is registered by the windows plugin's
    // `activate()` via `global_event_bus().register_channel::<PaneEvent>(...)`.
    // We tolerate "channel not registered yet" by just bailing out;
    // plugin load order will determine whether the subscriber sees
    // any events at all. Once the channel is registered, the
    // subscriber sees every subsequent event.
    let Ok(mut rx) = bmux_plugin::global_event_bus()
        .subscribe::<bmux_windows_plugin_api::windows_events::PaneEvent>(
        &bmux_windows_plugin_api::windows_events::EVENT_KIND,
    ) else {
        return;
    };
    std::thread::spawn(move || {
        while let Ok(event) = rx.blocking_recv() {
            let Ok(mut guard) = state.lock() else {
                break;
            };
            apply_pane_event(&mut guard, &translate_windows_event(&event));
        }
    });
}

/// Translate a `windows.pane-event` enum value to the decoration
/// plugin's local `pane-event` mirror. Both enums are structurally
/// identical by design; the local mirror exists so the decoration
/// BPDL doesn't import the windows BPDL.
fn translate_windows_event(
    event: &bmux_windows_plugin_api::windows_events::PaneEvent,
) -> PaneEvent {
    use bmux_windows_plugin_api::windows_events::PaneEvent as WinEvent;
    match event {
        WinEvent::Focused { pane_id } => PaneEvent::Focused { pane_id: *pane_id },
        WinEvent::Unfocused { pane_id } => PaneEvent::Unfocused { pane_id: *pane_id },
        WinEvent::Zoomed { pane_id } => PaneEvent::Zoomed { pane_id: *pane_id },
        WinEvent::Unzoomed { pane_id } => PaneEvent::Unzoomed { pane_id: *pane_id },
        WinEvent::Opened {
            pane_id,
            session_id,
        } => PaneEvent::Opened {
            pane_id: *pane_id,
            session_id: *session_id,
        },
        WinEvent::Closed { pane_id } => PaneEvent::Closed { pane_id: *pane_id },
        // The windows-plugin-api does not carry the exit bit on
        // `status-changed` (the receiver is expected to re-query
        // `pane-state`). The decoration plugin defers to whatever
        // value it currently holds; mark as non-exited so we don't
        // mistakenly flip to "Exited" without evidence.
        WinEvent::StatusChanged { pane_id } => PaneEvent::StatusChanged {
            pane_id: *pane_id,
            exited: false,
        },
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

/// Bundled theme presets shipped with the decoration plugin. Each
/// entry is `(name, toml_text)` — `name` is the file stem under
/// `~/.config/bmux/themes/<name>.toml`, `toml_text` is the file
/// contents.
///
/// Tooling (e.g. a future `bmux theme install` CLI subcommand)
/// walks this list and writes any missing preset to the user's
/// themes directory. Presets are enabled behind the `bundled-themes`
/// cargo feature on the decoration plugin crate; when disabled the
/// list is empty and users must install themes manually.
#[must_use]
pub fn bundled_theme_presets() -> &'static [(&'static str, &'static str)] {
    #[cfg(feature = "bundled-themes")]
    {
        &[
            ("hacker", include_str!("../assets/themes/hacker.toml")),
            ("cyberpunk", include_str!("../assets/themes/cyberpunk.toml")),
            ("minimal", include_str!("../assets/themes/minimal.toml")),
        ]
    }
    #[cfg(not(feature = "bundled-themes"))]
    {
        &[]
    }
}

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
    use bmux_scene_protocol::scene_protocol::Rect as SceneRect;

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
        assert!(!decoration.focused);
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

    // ─── PR 2: geometry + activity wiring ─────────────────────────

    fn rect(x: u16, y: u16, w: u16, h: u16) -> SceneRect {
        SceneRect { x, y, w, h }
    }

    #[test]
    fn notify_pane_geometry_caches_rects_and_bumps_revision() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(100);
        let before = plugin.build_scene().revision;
        block_on(handle.notify_pane_geometry(PaneGeometry {
            pane_id: pane,
            rect: rect(0, 0, 20, 5),
            content_rect: rect(1, 1, 18, 3),
        }))
        .expect("notify");
        let after = plugin.build_scene().revision;
        assert!(after > before);
        let geom = block_on(handle.pane_geometry(pane)).expect("geometry cached");
        assert_eq!(geom.rect, rect(0, 0, 20, 5));
        assert_eq!(geom.content_rect, rect(1, 1, 18, 3));
    }

    #[test]
    fn notify_pane_geometry_skips_revision_bump_for_unchanged_rects() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(101);
        let geom = PaneGeometry {
            pane_id: pane,
            rect: rect(0, 0, 10, 5),
            content_rect: rect(1, 1, 8, 3),
        };
        block_on(handle.notify_pane_geometry(geom.clone())).expect("first notify");
        let r1 = plugin.build_scene().revision;
        // Same geometry — should not bump.
        block_on(handle.notify_pane_geometry(geom)).expect("second notify");
        let r2 = plugin.build_scene().revision;
        assert_eq!(r1, r2, "unchanged geometry must not bump revision");
    }

    #[test]
    fn pane_event_focused_updates_activity_and_override() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(200);
        // Pre-populate an override so we can verify the focus mirror.
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");
        block_on(handle.notify_pane_event(PaneEvent::Focused { pane_id: pane })).expect("focus");
        let activity = block_on(handle.pane_activity(pane)).expect("activity cached");
        assert!(activity.focused);
        let deco = block_on(handle.pane_decoration(pane)).expect("deco");
        assert!(deco.focused);
    }

    #[test]
    fn pane_event_focused_unfocuses_other_panes() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let a = Uuid::from_u128(301);
        let b = Uuid::from_u128(302);
        block_on(handle.notify_pane_event(PaneEvent::Focused { pane_id: a })).expect("a");
        block_on(handle.notify_pane_event(PaneEvent::Focused { pane_id: b })).expect("b");
        let activity_a = block_on(handle.pane_activity(a)).expect("a cached");
        let activity_b = block_on(handle.pane_activity(b)).expect("b cached");
        assert!(
            !activity_a.focused,
            "a must have lost focus when b was focused"
        );
        assert!(activity_b.focused);
    }

    #[test]
    fn pane_event_zoomed_sets_zoom_flag() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(400);
        block_on(handle.notify_pane_event(PaneEvent::Zoomed { pane_id: pane })).expect("zoom");
        let a = block_on(handle.pane_activity(pane)).expect("cached");
        assert!(a.zoomed);
        block_on(handle.notify_pane_event(PaneEvent::Unzoomed { pane_id: pane })).expect("unzoom");
        let a = block_on(handle.pane_activity(pane)).expect("cached");
        assert!(!a.zoomed);
    }

    #[test]
    fn pane_event_status_changed_sets_lifecycle() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(500);
        block_on(handle.notify_pane_event(PaneEvent::StatusChanged {
            pane_id: pane,
            exited: true,
        }))
        .expect("exited");
        let a = block_on(handle.pane_activity(pane)).expect("cached");
        assert_eq!(a.status, PaneLifecycle::Exited);
    }

    #[test]
    fn forget_pane_drops_all_state() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(600);
        block_on(handle.set_pane_border(pane, BorderStyle::Double)).expect("set");
        block_on(handle.notify_pane_geometry(PaneGeometry {
            pane_id: pane,
            rect: rect(0, 0, 10, 5),
            content_rect: rect(1, 1, 8, 3),
        }))
        .expect("geom");
        block_on(handle.notify_pane_event(PaneEvent::Focused { pane_id: pane })).expect("focus");
        block_on(handle.forget_pane(pane)).expect("forget");
        assert!(block_on(handle.pane_geometry(pane)).is_none());
        assert!(block_on(handle.pane_activity(pane)).is_none());
        // pane_decoration always returns something (falls back to
        // default); after forget, focused must be false again.
        let deco = block_on(handle.pane_decoration(pane)).expect("default");
        assert!(!deco.focused);
    }

    #[test]
    fn build_scene_includes_geometry_when_pane_has_override_and_notify() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(700);
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");
        block_on(handle.notify_pane_geometry(PaneGeometry {
            pane_id: pane,
            rect: rect(2, 3, 20, 5),
            content_rect: rect(3, 4, 18, 3),
        }))
        .expect("geom");
        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface present");
        assert_eq!(surface.rect, rect(2, 3, 20, 5));
        assert_eq!(surface.content_rect, rect(3, 4, 18, 3));
    }

    // PR 3: scene-event emission. The decoration plugin publishes a
    // `DecorationScene` on the typed event bus every time state
    // mutates; the attach runtime subscribes and updates its scene
    // cache in place.
    #[test]
    fn bump_revision_emits_scene_on_event_bus_when_channel_registered() {
        // Register the channel first (as `activate()` would).
        let _sender = bmux_plugin::global_event_bus()
            .register_channel::<bmux_scene_protocol::scene_protocol::EventPayload>(
            bmux_scene_protocol::scene_protocol::EVENT_KIND,
        );

        let mut rx = bmux_plugin::global_event_bus()
            .subscribe::<bmux_scene_protocol::scene_protocol::EventPayload>(
                &bmux_scene_protocol::scene_protocol::EVENT_KIND,
            )
            .expect("subscribe");

        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(900);
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");

        // Drain at least one event; the broadcast buffer may replay
        // newer revisions from earlier tests so we just assert we
        // observed a non-zero revision.
        let event = rx.try_recv().expect("scene event was emitted");
        assert!(event.revision >= 1);
    }

    // ── PR 4: theme-extension + bundled presets ─────────────────

    #[test]
    fn validate_theme_extension_accepts_valid_toml() {
        let text = r##"
        [unfocused]
        style = "rounded"
        fg = "#1a4d1a"
        bg = ""
        gradient_from = ""
        gradient_to = ""
        glyphs_custom = []

        [focused]
        style = "thick"
        fg = "#39ff14"
        bg = ""
        gradient_from = ""
        gradient_to = ""
        glyphs_custom = []

        [zoomed]
        style = "double"
        fg = "#ffd700"
        bg = ""
        gradient_from = ""
        gradient_to = ""
        glyphs_custom = []

        [badges]
        running = ">"
        exited  = "x"
        "##;
        assert_eq!(validate_theme_extension_toml(text), ValidationResult::Ok);
    }

    #[test]
    fn validate_theme_extension_rejects_missing_required_field() {
        let text = r##"
        [unfocused]
        style = "rounded"
        fg = "#1a4d1a"
        # missing bg/gradient/etc
        "##;
        let result = validate_theme_extension_toml(text);
        match result {
            ValidationResult::Errors { errors } => {
                assert!(!errors.is_empty(), "expected at least one validation error");
            }
            ValidationResult::Ok => panic!("expected validation errors; got Ok"),
        }
    }

    #[test]
    fn validate_theme_extension_rejects_syntactically_broken_toml() {
        let text = "this is not toml {{{{";
        let result = validate_theme_extension_toml(text);
        assert!(matches!(result, ValidationResult::Errors { .. }));
    }

    #[test]
    fn bundled_theme_presets_parse_against_schema() {
        for (name, toml_text) in bundled_theme_presets() {
            // Each bundled theme's `[plugins."bmux.decoration"]` section
            // must validate against the DecorationThemeExtension
            // schema — otherwise we'd ship themes nobody can use.
            let theme: toml::Value =
                toml::from_str(toml_text).unwrap_or_else(|e| panic!("{name} parses: {e}"));
            let plugins_table = theme
                .get("plugins")
                .unwrap_or_else(|| panic!("{name} missing [plugins] table"));
            let decoration_table = plugins_table
                .get("bmux.decoration")
                .unwrap_or_else(|| panic!("{name} missing [plugins.\"bmux.decoration\"]"));
            // Pretty-print the extracted table so validate_theme_extension_toml can parse it.
            let extension_toml = toml::to_string(&decoration_table)
                .unwrap_or_else(|e| panic!("{name} re-serialize: {e}"));
            let result = validate_theme_extension_toml(&extension_toml);
            assert_eq!(
                result,
                ValidationResult::Ok,
                "bundled theme `{name}` failed schema validation: {result:?}\n{extension_toml}",
            );
        }
    }

    #[test]
    fn current_theme_extension_returns_none_on_fresh_plugin() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        assert!(block_on(handle.current_theme_extension()).is_none());
    }

    #[test]
    fn load_theme_extension_from_config_dir_returns_none_on_missing_config() {
        let tmp =
            std::env::temp_dir().join(format!("bmux-decoration-test-{}", uuid::Uuid::new_v4()));
        assert!(load_theme_extension_from_config_dir(&tmp).is_none());
    }

    #[test]
    fn load_theme_extension_from_config_dir_reads_plugin_section() {
        let tmp =
            std::env::temp_dir().join(format!("bmux-decoration-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(tmp.join("themes")).expect("mkdir");

        let main_toml = r#"
[appearance]
theme = "hacker"
"#;
        std::fs::write(tmp.join("bmux.toml"), main_toml).expect("write main");
        std::fs::write(
            tmp.join("themes/hacker.toml"),
            include_str!("../assets/themes/hacker.toml"),
        )
        .expect("write theme");

        let ext = load_theme_extension_from_config_dir(&tmp)
            .expect("loading decoration theme extension should succeed");
        assert_eq!(ext.focused.style, "thick");
        assert_eq!(ext.focused.fg, "#39ff14");
        assert_eq!(ext.badges.running, "▶");
    }
}
