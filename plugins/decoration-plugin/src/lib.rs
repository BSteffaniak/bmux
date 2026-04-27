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

pub mod glyphs;
pub mod scripting;

use std::collections::{BTreeMap, HashMap, VecDeque, hash_map::DefaultHasher};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use bmux_decoration_plugin_api::decoration_state::{
    BorderSpec, BorderStyle, DecorationEvent, DecorationStateService, DecorationThemeExtension,
    INTERFACE_ID as DECORATION_STATE_INTERFACE_ID, NotifyError, PaneActivity, PaneDecoration,
    PaneEvent, PaneGeometry, PaneLifecycle, SetStyleError, ValidationError, ValidationResult,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs, Color, DecorationScene, GradientAxis, InteractiveRegion, NamedColor,
    PaintCommand, Rect, Style, SurfaceDecoration,
};
use uuid::Uuid;

use crate::scripting::{
    PerfTracker, ScriptBackend, ScriptEventMessage, ScriptMessage, ScriptRenderMessage,
    bundled_decoration_scripts,
};

/// In-memory state store.
#[derive(Default)]
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
    /// Currently-loaded extension supplied through `theme-extension:apply`;
    /// `None` means "no extension observed; paint with built-in ASCII defaults".
    current_theme: Option<DecorationThemeExtension>,
    /// Compiled decoration script, if any. `None` means the theme
    /// did not request a script, or scripting was disabled at build
    /// time, or compilation failed (the loader logs the failure).
    script_backend: Option<Box<dyn ScriptBackend>>,
    /// Display path of the active script (used for perf + error
    /// messages). `None` when no script is loaded.
    script_path: Option<PathBuf>,
    /// Fingerprint of the active script source. Used to preserve the
    /// live Lua VM when a theme preview and final selection apply the
    /// same script back-to-back.
    script_source_hash: Option<u64>,
    /// Monotonic start instant used to populate render-message `time_ms`.
    /// Set when the first script is installed so relative timings are
    /// stable across reloads.
    script_started_at: Option<Instant>,
    /// Monotonic frame counter passed to the script each invocation.
    script_frame: u64,
    /// Optional perf tracker that emits a `WARN` log when the script's
    /// P95 invoke time drifts above the threshold.
    script_perf: Option<PerfTracker>,
    /// Pending event messages to deliver into the Lua VM before the next render.
    script_events: VecDeque<ScriptEventMessage>,
    /// External plugin event kinds the active script asked to receive.
    script_event_subscriptions: Vec<String>,
    /// Active animation tick rate. Threads exit when this value changes.
    animation_hz: Option<u16>,
    /// Diagnostic flag flipped on the first frame where the script was
    /// actually invoked against at least one pane's geometry. Paired
    /// with a one-shot info log so we can confirm the full
    /// load-compile-geometry-invoke chain during debugging. Reset on
    /// plugin activation (implicit via `State::default`).
    script_first_invoke_logged: bool,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Box<dyn ScriptBackend>` is not `Debug`, so derive(Debug) is
        // off. We still want inspector-friendly output for the rest.
        f.debug_struct("State")
            .field("panes", &self.panes)
            .field("geometry", &self.geometry)
            .field("activity", &self.activity)
            .field("default_border", &self.default_border)
            .field("scene_revision", &self.scene_revision)
            .field("current_theme", &self.current_theme)
            .field("script_path", &self.script_path)
            .field("script_source_hash", &self.script_source_hash)
            .field("script_frame", &self.script_frame)
            .field("animation_hz", &self.animation_hz)
            .finish_non_exhaustive()
    }
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
                .map_or_else(|_| empty_scene(), |mut state| build_scene(&mut state))
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

    fn apply_theme_extension<'a>(
        &'a self,
        toml_text: String,
        config_dir_candidates: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ValidationResult>> + Send + 'a>> {
        Box::pin(async move {
            let candidates = config_dir_candidates
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            apply_theme_extension_toml(&self.state, &toml_text, &candidates)
        })
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
/// Build a [`DecorationScene`] from the plugin's current state.
///
/// The authoritative set of panes is `state.geometry`: every pane the
/// attach runtime has reported gets an explicit `SurfaceDecoration`
/// entry, even when no script is loaded and no per-pane override
/// exists. The paint-command vector for each surface is resolved in
/// priority order:
///
/// 1. If the pane has an explicit override in `state.panes` (set via
///    `set-pane-border` IPC), honour that override's glyph choice.
/// 2. Else if a theme is loaded, pick `focused` / `zoomed` /
///    `unfocused` based on `state.activity` and emit paint commands
///    driven by the theme's [`BorderSpec`].
/// 3. Else emit a built-in default (rounded glyphs, bright-white for
///    the focused pane, white for unfocused).
///
/// After surfaces are pre-populated, `merge_script_paint_commands`
/// runs and layers the active decoration script's paint commands on
/// top at higher `z` values.
fn build_scene(state: &mut State) -> DecorationScene {
    let mut surfaces = BTreeMap::new();
    let pane_ids: Vec<Uuid> = state.geometry.keys().copied().collect();
    for pane_id in pane_ids {
        let Some(geom) = state.geometry.get(&pane_id).cloned() else {
            continue;
        };
        let (focused, zoomed) = state
            .activity
            .get(&pane_id)
            .map_or((false, false), |a| (a.focused, a.zoomed));
        let rect = geom.rect.clone();
        let content_rect = geom.content_rect.clone();

        let paint_commands = if let Some(override_entry) = state.panes.get(&pane_id) {
            paint_commands_from_override(override_entry.border, focused, &rect)
        } else if let Some(theme) = state.current_theme.as_ref() {
            let spec = theme_border_spec_for(theme, focused, zoomed);
            paint_commands_from_border_spec(spec, &rect)
        } else {
            paint_commands_default(focused, &rect)
        };

        // Every pane with a visible border contributes four
        // interactive regions (one per edge). The attach runtime
        // merges these into the AttachScene's per-surface regions so
        // core mouse dispatch can route border clicks back to the
        // decoration plugin without needing to know anything about
        // decoration internals.
        let interactive_regions = border_interactive_regions(&rect);

        surfaces.insert(
            pane_id,
            SurfaceDecoration {
                surface_id: pane_id,
                rect,
                content_rect,
                paint_commands,
                interactive_regions,
            },
        );
    }
    merge_script_paint_commands(state, &mut surfaces);
    DecorationScene {
        revision: state.scene_revision,
        surfaces,
        animation: None,
    }
}

fn script_pane_payload(state: &State, pane_id: Uuid) -> Option<serde_json::Value> {
    let geom = state.geometry.get(&pane_id)?;
    let activity = state.activity.get(&pane_id);
    let (focused, zoomed) = activity.map_or((false, false), |a| (a.focused, a.zoomed));
    let status = activity.map_or(PaneLifecycle::Running, |a| a.status);
    Some(serde_json::json!({
        "id": pane_id.to_string(),
        "rect": rect_json(&geom.rect),
        "content_rect": rect_json(&geom.content_rect),
        "focused": focused,
        "zoomed": zoomed,
        "status": match status {
            PaneLifecycle::Running => "running",
            PaneLifecycle::Exited => "exited",
        },
    }))
}

fn rect_json(rect: &Rect) -> serde_json::Value {
    serde_json::json!({
        "x": rect.x,
        "y": rect.y,
        "w": rect.w,
        "h": rect.h,
    })
}

/// Deliver pending script events, invoke one render message, and merge returned
/// surface paint commands. Render messages carry the current panes because pane
/// geometry/activity are render inputs; event messages remain for plugin-defined
/// signals that scripts want to cache independently.
fn merge_script_paint_commands(
    state: &mut State,
    surfaces: &mut BTreeMap<Uuid, SurfaceDecoration>,
) {
    let Some(backend) = state.script_backend.as_ref() else {
        return;
    };
    state.script_frame = state.script_frame.saturating_add(1);
    let is_first_frame = state.script_frame == 1;
    let geometry_count = state.geometry.len();
    let mut invoked = 0_usize;
    let mut commands_merged = 0_usize;

    while let Some(event) = state.script_events.pop_front() {
        invoked += 1;
        let message = ScriptMessage::Event(event);
        let outcome = match backend.invoke(&message) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    target: "decoration.script",
                    error = %e,
                    "decoration script event invocation failed",
                );
                continue;
            }
        };
        record_script_perf(state, outcome.duration);
    }

    let started_at = state.script_started_at;
    let time_ms = started_at.map_or(0, |started_at| {
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    });
    invoked += 1;
    let render = ScriptMessage::Render(ScriptRenderMessage {
        time_ms,
        frame: state.script_frame,
        panes: script_panes_payload(state),
    });
    let outcome = match backend.invoke(&render) {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                target: "decoration.script",
                error = %error,
                "decoration script render invocation failed",
            );
            return;
        }
    };
    record_script_perf(state, outcome.duration);
    for (pane_id, commands) in outcome.surfaces {
        let Ok(pane_id) = pane_id.parse::<Uuid>() else {
            tracing::warn!(target: "decoration.script", pane_id, "script returned unknown pane id");
            continue;
        };
        commands_merged += commands.len();
        let surface = surfaces
            .entry(pane_id)
            .or_insert_with(|| empty_surface_for(state, pane_id));
        surface.paint_commands.extend(commands);
    }

    if is_first_frame {
        tracing::debug!(
            geometry_count = geometry_count,
            invoked = invoked,
            commands_merged = commands_merged,
            "first decoration script merge complete",
        );
    }
    if !state.script_first_invoke_logged && invoked > 0 {
        state.script_first_invoke_logged = true;
        tracing::debug!(
            geometry_count = geometry_count,
            invoked = invoked,
            commands_merged = commands_merged,
            "first decoration script invocation with geometry",
        );
    }
}

fn script_panes_payload(state: &State) -> serde_json::Value {
    serde_json::Value::Array(
        state
            .geometry
            .keys()
            .filter_map(|pane_id| script_pane_payload(state, *pane_id))
            .collect(),
    )
}

fn record_script_perf(state: &State, duration: Duration) {
    if let Some(tracker) = state.script_perf.as_ref()
        && let Some(msg) = tracker.record(duration)
    {
        tracing::warn!(target: "decoration.script", "{msg}");
    }
}

fn empty_surface_for(state: &State, pane_id: Uuid) -> SurfaceDecoration {
    let (rect, content_rect) = state.geometry.get(&pane_id).map_or_else(
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
        paint_commands: Vec::new(),
        interactive_regions: Vec::new(),
    }
}

/// Identifier the decoration plugin attributes to its own
/// interactive regions. Used by the attach runtime's mouse hit-test
/// to route clicks on decoration chrome back to this plugin.
const DECORATION_PLUGIN_ID: &str = "bmux.decoration";

/// Build the four edge regions (top / bottom / left / right) of a
/// pane border as [`InteractiveRegion`]s owned by the decoration
/// plugin. Returns an empty vec for rects too small to carry a
/// border (fewer than 2 cells on either axis).
fn border_interactive_regions(rect: &Rect) -> Vec<InteractiveRegion> {
    if rect.w < 2 || rect.h < 2 {
        return Vec::new();
    }
    let last_y = rect.y.saturating_add(rect.h.saturating_sub(1));
    let last_x = rect.x.saturating_add(rect.w.saturating_sub(1));
    vec![
        InteractiveRegion {
            rect: Rect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: 1,
            },
            region_id: "border-top".to_string(),
            owning_plugin_id: DECORATION_PLUGIN_ID.to_string(),
        },
        InteractiveRegion {
            rect: Rect {
                x: rect.x,
                y: last_y,
                w: rect.w,
                h: 1,
            },
            region_id: "border-bottom".to_string(),
            owning_plugin_id: DECORATION_PLUGIN_ID.to_string(),
        },
        InteractiveRegion {
            rect: Rect {
                x: rect.x,
                y: rect.y,
                w: 1,
                h: rect.h,
            },
            region_id: "border-left".to_string(),
            owning_plugin_id: DECORATION_PLUGIN_ID.to_string(),
        },
        InteractiveRegion {
            rect: Rect {
                x: last_x,
                y: rect.y,
                w: 1,
                h: rect.h,
            },
            region_id: "border-right".to_string(),
            owning_plugin_id: DECORATION_PLUGIN_ID.to_string(),
        },
    ]
}

/// Fallback scene returned when the state lock is poisoned.
fn empty_scene() -> DecorationScene {
    DecorationScene {
        revision: 0,
        surfaces: BTreeMap::new(),
        animation: None,
    }
}

/// Bump the scene revision and publish the updated [`DecorationScene`] as
/// retained state on the typed plugin event bus. Called from every mutator so
/// consumers (e.g. the attach runtime) can refresh their scene cache
/// incrementally while late subscribers can still hydrate from the current
/// value. Publication silently no-ops if the event-bus channel has not been
/// registered yet (the decoration plugin registers it in
/// [`DecorationPlugin::activate`]).
fn bump_revision(state: &mut State) {
    state.scene_revision = state.scene_revision.saturating_add(1);
    // Build + publish while we still hold the lock: this keeps the revision
    // monotonic from subscribers' perspective.
    let scene = build_scene(state);
    let _ = bmux_plugin::global_event_bus()
        .publish_state(&bmux_scene_protocol::scene_protocol::STATE_KIND, scene);
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

fn apply_theme_extension_toml(
    state: &Arc<Mutex<State>>,
    text: &str,
    config_dir_candidates: &[PathBuf],
) -> Result<(), ValidationResult> {
    if text.trim().is_empty() {
        if let Ok(mut state) = state.lock() {
            state.current_theme = None;
            state.animation_hz = None;
            install_script_backend(&mut state, None);
            bump_revision(&mut state);
            tracing::info!(
                scene_revision = state.scene_revision,
                "decoration theme extension cleared",
            );
        }
        return Ok(());
    }

    let parsed = toml::from_str::<toml::Value>(text).map_err(|err| ValidationResult::Errors {
        errors: vec![ValidationError {
            path: "<root>".to_string(),
            message: format!("TOML parse error: {err}"),
        }],
    })?;
    let extension: DecorationThemeExtension =
        parsed.try_into().map_err(|err| ValidationResult::Errors {
            errors: vec![ValidationError {
                path: "<schema>".to_string(),
                message: err.to_string(),
            }],
        })?;
    let script = extension
        .script
        .as_deref()
        .and_then(|spec| resolve_decoration_script(config_dir_candidates, spec));
    let animation_hz = extension.animation.as_ref().map(|animation| animation.hz);
    if let Ok(mut state) = state.lock() {
        state.current_theme = Some(extension);
        state.animation_hz = animation_hz;
        install_script_backend(&mut state, script);
        bump_revision(&mut state);
        tracing::info!(
            scene_revision = state.scene_revision,
            animation_hz = state.animation_hz,
            script_loaded = state.script_backend.is_some(),
            "decoration theme extension applied",
        );
    }
    if let Some(hz) = animation_hz
        && hz > 0
    {
        spawn_animation_tick_thread(Arc::downgrade(state), hz);
    }
    Ok(())
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
            .map_or_else(|_| empty_scene(), |mut state| build_scene(&mut state))
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

/// Parse a `#rrggbb` hex colour string into [`Color::Rgb`]. Returns
/// `None` for empty strings, missing `#` prefix, non-hex digits, or
/// wrong-length inputs. Callers use this to resolve theme spec
/// colours.
fn parse_hex_color(s: &str) -> Option<Color> {
    let trimmed = s.trim();
    let hex = trimmed.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
    let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
    let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
    Some(Color::Rgb { r, g, b })
}

/// Parse a theme's `gradient-axis` string into the scene-protocol
/// enum. Accepts `kebab-case`, `snake_case`, and mixed case; empty
/// string and unknown values default to [`GradientAxis::Horizontal`]
/// to match historical behaviour.
fn parse_gradient_axis(s: &str) -> GradientAxis {
    let normalized = s.trim().to_ascii_lowercase().replace('-', "_");
    match normalized.as_str() {
        "vertical" => GradientAxis::Vertical,
        "diagonal" => GradientAxis::Diagonal,
        _ => GradientAxis::Horizontal,
    }
}

/// Pick the per-focus/per-zoom [`BorderSpec`] from a theme. Zoom wins
/// over focus (a zoomed pane is always focused by construction, but
/// the zoom style takes precedence).
fn theme_border_spec_for(
    theme: &DecorationThemeExtension,
    focused: bool,
    zoomed: bool,
) -> &BorderSpec {
    if zoomed {
        &theme.zoomed
    } else if focused {
        &theme.focused
    } else {
        &theme.unfocused
    }
}

/// Map the `decoration-state::border-style` enum (used by
/// `set-pane-border` IPC) onto a [`BorderGlyphs`] preset. Explicit
/// overrides honour the user's glyph choice but otherwise derive
/// their style from the focused/unfocused named-colour pair used by
/// the no-theme default.
fn border_style_to_glyphs(border: BorderStyle) -> BorderGlyphs {
    match border {
        BorderStyle::None => BorderGlyphs::None,
        BorderStyle::Ascii => BorderGlyphs::Ascii,
        BorderStyle::Single => BorderGlyphs::SingleLine,
        BorderStyle::Double => BorderGlyphs::DoubleLine,
    }
}

/// Build a [`Style`] whose only populated field is `fg`. Used by the
/// gradient-border constructor so each `GradientRun`/`CellGrid` cell
/// carries the per-position colour without inheriting bold/underline.
fn solid_fg_style(fg: Color) -> Style {
    Style {
        fg: Some(fg),
        bg: None,
        bold: false,
        underline: false,
        italic: false,
        reverse: false,
        dim: false,
        blink: false,
        strikethrough: false,
    }
}

/// Linear-interpolate two [`Color::Rgb`] endpoints. Returns the `from`
/// colour when either input isn't RGB (theme gradient endpoints are
/// always hex strings in practice; other colour modes can't
/// interpolate meaningfully).
fn lerp_rgb(from: &Color, to: &Color, t: f32) -> Color {
    let (
        &Color::Rgb {
            r: fr,
            g: fg,
            b: fb,
        },
        &Color::Rgb {
            r: tr,
            g: tg,
            b: tb,
        },
    ) = (from, to)
    else {
        return from.clone();
    };
    let blend = |a: u8, b: u8| -> u8 {
        let v = f32::from(a) + (f32::from(b) - f32::from(a)) * t.clamp(0.0, 1.0);
        // Clamp to u8 range before cast; cast is safe because
        // `clamp(0.0, 255.0)` guarantees the value is in [0, 255].
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let out = v.round().clamp(0.0, 255.0) as u8;
        out
    };
    Color::Rgb {
        r: blend(fr, tr),
        g: blend(fg, tg),
        b: blend(fb, tb),
    }
}

/// No-theme default paint commands: rounded glyphs, bright-white for
/// the focused pane, white for the unfocused pane, bold on focused.
fn paint_commands_default(focused: bool, rect: &Rect) -> Vec<PaintCommand> {
    if rect.w < 2 || rect.h < 2 {
        return Vec::new();
    }
    let fg = Color::Named {
        name: if focused {
            NamedColor::BrightWhite
        } else {
            NamedColor::White
        },
    };
    let style = Style {
        fg: Some(fg),
        bg: None,
        bold: focused,
        underline: false,
        italic: false,
        reverse: false,
        dim: false,
        blink: false,
        strikethrough: false,
    };
    vec![PaintCommand::BoxBorder {
        rect: rect.clone(),
        z: 0,
        glyphs: BorderGlyphs::Rounded,
        style,
    }]
}

/// Paint commands for an explicit `set-pane-border` override. The
/// user picked this glyph set directly, so we emit a single
/// [`PaintCommand::BoxBorder`] with the matching preset and the
/// focused/unfocused colour pair used by the no-theme default.
fn paint_commands_from_override(
    border: BorderStyle,
    focused: bool,
    rect: &Rect,
) -> Vec<PaintCommand> {
    if matches!(border, BorderStyle::None) || rect.w < 2 || rect.h < 2 {
        return Vec::new();
    }
    let glyphs = border_style_to_glyphs(border);
    let fg = Color::Named {
        name: if focused {
            NamedColor::BrightWhite
        } else {
            NamedColor::White
        },
    };
    let style = Style {
        fg: Some(fg),
        bg: None,
        bold: focused,
        underline: false,
        italic: false,
        reverse: false,
        dim: false,
        blink: false,
        strikethrough: false,
    };
    vec![PaintCommand::BoxBorder {
        rect: rect.clone(),
        z: 0,
        glyphs,
        style,
    }]
}

/// Resolve a theme's [`BorderSpec`] into concrete paint commands for a
/// pane of `rect` size. Picks between flat colour (single
/// `BoxBorder`) and gradient (multiple `GradientRun` / `CellGrid`
/// commands depending on `gradient-axis`) based on whether both
/// `gradient-from` and `gradient-to` parse as hex colours.
fn paint_commands_from_border_spec(spec: &BorderSpec, rect: &Rect) -> Vec<PaintCommand> {
    if rect.w < 2 || rect.h < 2 {
        return Vec::new();
    }
    let glyphs = if spec.style.eq_ignore_ascii_case("custom") {
        crate::glyphs::parse_custom_glyphs(&spec.glyphs_custom)
    } else {
        crate::glyphs::parse_border_glyphs(&spec.style)
    };
    if matches!(glyphs, BorderGlyphs::None) {
        return Vec::new();
    }
    let grad_from = parse_hex_color(&spec.gradient_from);
    let grad_to = parse_hex_color(&spec.gradient_to);
    if let (Some(from), Some(to)) = (grad_from, grad_to) {
        let axis = parse_gradient_axis(&spec.gradient_axis);
        return paint_commands_gradient_border(rect, &glyphs, &from, &to, axis);
    }
    let style = Style {
        fg: parse_hex_color(&spec.fg),
        bg: parse_hex_color(&spec.bg),
        bold: false,
        underline: false,
        italic: false,
        reverse: false,
        dim: false,
        blink: false,
        strikethrough: false,
    };
    vec![PaintCommand::BoxBorder {
        rect: rect.clone(),
        z: 0,
        glyphs,
        style,
    }]
}

/// Paint a gradient box border by emitting four [`PaintCommand`]s
/// (top, bottom, left, right edges) whose styles interpolate from
/// `from` to `to` along `axis`.
///
/// Behaviour per axis:
/// - `Horizontal` — top and bottom edges carry a `GradientRun` that
///   interpolates along the edge from `from` (left) to `to` (right);
///   left edge is a flat `GradientRun` at `from`; right edge is a
///   flat `GradientRun` at `to`. This gives a CSS
///   `linear-gradient(to right, from, to)` look.
/// - `Vertical` — symmetric: top flat at `from`, bottom flat at
///   `to`, left and right gradient top-to-bottom.
/// - `Diagonal` — every border cell is painted explicitly with its
///   own interpolated colour via per-edge [`PaintCommand::CellGrid`]
///   commands. Corners meet at their natural diagonal positions.
#[allow(clippy::too_many_lines)] // Explicit per-axis match: splitting further obscures the variant-dispatch shape.
fn paint_commands_gradient_border(
    rect: &Rect,
    glyphs: &BorderGlyphs,
    from: &Color,
    to: &Color,
    axis: GradientAxis,
) -> Vec<PaintCommand> {
    let Some(corners) = bmux_scene_protocol::glyphs::border_glyphs_corners_or_custom(glyphs) else {
        return Vec::new();
    };
    let w = rect.w;
    let h = rect.h;
    if w < 2 || h < 2 {
        return Vec::new();
    }
    let from_style = solid_fg_style(from.clone());
    let to_style = solid_fg_style(to.clone());
    let last_x = rect.x.saturating_add(w.saturating_sub(1));
    let last_y = rect.y.saturating_add(h.saturating_sub(1));

    let horizontal = String::from(corners.horizontal);
    let vertical = String::from(corners.vertical);

    match axis {
        GradientAxis::Horizontal => {
            let top_text = build_edge_text(corners.top_left, &horizontal, corners.top_right, w);
            let bottom_text =
                build_edge_text(corners.bottom_left, &horizontal, corners.bottom_right, w);
            let mut commands = Vec::with_capacity(4);
            commands.push(PaintCommand::GradientRun {
                col: rect.x,
                row: rect.y,
                z: 0,
                text: top_text,
                axis: GradientAxis::Horizontal,
                from_style: from_style.clone(),
                to_style: to_style.clone(),
            });
            commands.push(PaintCommand::GradientRun {
                col: rect.x,
                row: last_y,
                z: 0,
                text: bottom_text,
                axis: GradientAxis::Horizontal,
                from_style: from_style.clone(),
                to_style: to_style.clone(),
            });
            if h > 2 {
                let side_len = usize::from(h.saturating_sub(2));
                let side_text = vertical.repeat(side_len);
                commands.push(PaintCommand::GradientRun {
                    col: rect.x,
                    row: rect.y.saturating_add(1),
                    z: 0,
                    text: side_text.clone(),
                    axis: GradientAxis::Vertical,
                    from_style: from_style.clone(),
                    to_style: from_style.clone(),
                });
                commands.push(PaintCommand::GradientRun {
                    col: last_x,
                    row: rect.y.saturating_add(1),
                    z: 0,
                    text: side_text,
                    axis: GradientAxis::Vertical,
                    from_style: to_style.clone(),
                    to_style,
                });
            }
            commands
        }
        GradientAxis::Vertical => {
            let top_text = build_edge_text(corners.top_left, &horizontal, corners.top_right, w);
            let bottom_text =
                build_edge_text(corners.bottom_left, &horizontal, corners.bottom_right, w);
            let mut commands = Vec::with_capacity(4);
            commands.push(PaintCommand::GradientRun {
                col: rect.x,
                row: rect.y,
                z: 0,
                text: top_text,
                axis: GradientAxis::Horizontal,
                from_style: from_style.clone(),
                to_style: from_style.clone(),
            });
            commands.push(PaintCommand::GradientRun {
                col: rect.x,
                row: last_y,
                z: 0,
                text: bottom_text,
                axis: GradientAxis::Horizontal,
                from_style: to_style.clone(),
                to_style: to_style.clone(),
            });
            if h > 2 {
                let side_len = usize::from(h.saturating_sub(2));
                let side_text = vertical.repeat(side_len);
                commands.push(PaintCommand::GradientRun {
                    col: rect.x,
                    row: rect.y.saturating_add(1),
                    z: 0,
                    text: side_text.clone(),
                    axis: GradientAxis::Vertical,
                    from_style: from_style.clone(),
                    to_style: to_style.clone(),
                });
                commands.push(PaintCommand::GradientRun {
                    col: last_x,
                    row: rect.y.saturating_add(1),
                    z: 0,
                    text: side_text,
                    axis: GradientAxis::Vertical,
                    from_style,
                    to_style,
                });
            }
            commands
        }
        GradientAxis::Diagonal => paint_commands_diagonal_gradient_border(rect, &corners, from, to),
    }
}

/// Build the full horizontal edge string (top or bottom) given its
/// corner glyphs and the horizontal run glyph. Widths < 2 are caller-
/// filtered; width == 2 yields just the two corners.
fn build_edge_text(left: &str, mid: &str, right: &str, width: u16) -> String {
    let w = usize::from(width);
    if w == 0 {
        return String::new();
    }
    if w == 1 {
        return left.to_string();
    }
    let body_len = mid.len() * w.saturating_sub(2);
    let mut out = String::with_capacity(left.len() + body_len + right.len());
    out.push_str(left);
    if w > 2 {
        for _ in 0..(w - 2) {
            out.push_str(mid);
        }
    }
    out.push_str(right);
    out
}

/// Per-cell diagonal gradient: each border cell is painted via a
/// [`PaintCommand::CellGrid`] entry whose style carries its own
/// lerped colour. Produces one `CellGrid` per edge so the renderer
/// can diff them independently.
fn paint_commands_diagonal_gradient_border(
    rect: &Rect,
    corners: &bmux_scene_protocol::glyphs::BorderGlyphSet<'_>,
    from: &Color,
    to: &Color,
) -> Vec<PaintCommand> {
    use bmux_scene_protocol::scene_protocol::Cell;
    let w = rect.w;
    let h = rect.h;
    if w < 2 || h < 2 {
        return Vec::new();
    }
    // `t` at (dx, dy) = (dx + dy) / (w + h - 2). Corners land at
    // (0, 0) -> t=0 and (w-1, h-1) -> t=1.
    let denom = f32::from(w.saturating_sub(1).saturating_add(h.saturating_sub(1))).max(1.0);
    let cell_at = |dx: u16, dy: u16, glyph: &str| -> Cell {
        #[allow(clippy::cast_precision_loss)]
        let t = (f32::from(dx) + f32::from(dy)) / denom;
        let color = lerp_rgb(from, to, t);
        Cell {
            glyph: glyph.to_string(),
            style: solid_fg_style(color),
        }
    };
    let mut commands = Vec::new();

    // Top edge: top_left + horizontal*(w-2) + top_right, all at dy=0.
    let mut top_cells = Vec::with_capacity(usize::from(w));
    for dx in 0..w {
        let glyph = if dx == 0 {
            corners.top_left
        } else if dx == w - 1 {
            corners.top_right
        } else {
            corners.horizontal
        };
        top_cells.push(cell_at(dx, 0, glyph));
    }
    commands.push(PaintCommand::CellGrid {
        origin_col: rect.x,
        origin_row: rect.y,
        z: 0,
        cols: w,
        cells: top_cells,
    });

    // Bottom edge at dy=h-1.
    let mut bottom_cells = Vec::with_capacity(usize::from(w));
    for dx in 0..w {
        let glyph = if dx == 0 {
            corners.bottom_left
        } else if dx == w - 1 {
            corners.bottom_right
        } else {
            corners.horizontal
        };
        bottom_cells.push(cell_at(dx, h - 1, glyph));
    }
    commands.push(PaintCommand::CellGrid {
        origin_col: rect.x,
        origin_row: rect.y.saturating_add(h - 1),
        z: 0,
        cols: w,
        cells: bottom_cells,
    });

    // Left and right edges skip corners (already painted above).
    if h > 2 {
        let side_len = usize::from(h - 2);
        let mut left_cells = Vec::with_capacity(side_len);
        let mut right_cells = Vec::with_capacity(side_len);
        for dy in 1..(h - 1) {
            left_cells.push(cell_at(0, dy, corners.vertical));
            right_cells.push(cell_at(w - 1, dy, corners.vertical));
        }
        commands.push(PaintCommand::CellGrid {
            origin_col: rect.x,
            origin_row: rect.y.saturating_add(1),
            z: 0,
            cols: 1,
            cells: left_cells,
        });
        commands.push(PaintCommand::CellGrid {
            origin_col: rect.x.saturating_add(w - 1),
            origin_row: rect.y.saturating_add(1),
            z: 0,
            cols: 1,
            cells: right_cells,
        });
    }
    commands
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

/// Resolved decoration script: the canonical path used for error
/// reporting plus the source string handed to the Lua backend. The
/// path is synthetic (`bundled:<name>`) for bundled scripts and
/// filesystem-absolute for user-authored scripts.
#[derive(Debug, Clone)]
struct ResolvedScript {
    path: PathBuf,
    source: String,
}

/// Resolve a `script = "..."` theme value into a concrete source
/// string. Resolution rules (first match wins):
///
/// 1. An absolute path is read directly from the filesystem.
/// 2. A relative path containing `/` or `.` is probed against each
///    candidate config dir in order, returning the first readable
///    match.
/// 3. A bare stem (no slashes, no dots) matches a bundled script by
///    name (`"pulse"` -> `pulse.lua`).
///
/// Returns `None` (and logs a warning) when no match produces a
/// readable script.
fn resolve_decoration_script(
    config_dir_candidates: &[PathBuf],
    spec: &str,
) -> Option<ResolvedScript> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    let looks_like_path = trimmed.contains('/') || trimmed.contains('.');
    if looks_like_path {
        if std::path::Path::new(trimmed).is_absolute() {
            let candidate = PathBuf::from(trimmed);
            return match std::fs::read_to_string(&candidate) {
                Ok(source) => Some(ResolvedScript {
                    path: candidate,
                    source,
                }),
                Err(err) => {
                    tracing::warn!(
                        target: "decoration.script",
                        path = ?candidate,
                        error = %err,
                        "failed to read decoration script from theme; decorations fall back to defaults",
                    );
                    None
                }
            };
        }
        // Relative path — probe each candidate config dir.
        let mut last_error: Option<(PathBuf, std::io::Error)> = None;
        for dir in config_dir_candidates {
            let candidate = dir.join(trimmed);
            match std::fs::read_to_string(&candidate) {
                Ok(source) => {
                    return Some(ResolvedScript {
                        path: candidate,
                        source,
                    });
                }
                Err(err) => last_error = Some((candidate, err)),
            }
        }
        if let Some((path, err)) = last_error {
            tracing::warn!(
                target: "decoration.script",
                path = ?path,
                error = %err,
                "failed to read decoration script from any config dir candidate; decorations fall back to defaults",
            );
        }
        return None;
    }
    // Bare stem — try bundled scripts.
    for (name, source) in bundled_decoration_scripts() {
        if *name == trimmed {
            return Some(ResolvedScript {
                path: PathBuf::from(format!("bundled:{name}")),
                source: (*source).to_string(),
            });
        }
    }
    tracing::warn!(
        target: "decoration.script",
        script = %trimmed,
        "decoration script not found (neither filesystem path nor bundled name)",
    );
    None
}

impl RustPlugin for DecorationPlugin {
    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        // Register the retained scene channel before any mutator (including
        // the initial revision bump below) tries to publish. Failure is
        // non-fatal — the channel may already exist from a prior load;
        // `bump_revision` tolerates a missing channel.
        let _ = bmux_plugin::global_event_bus()
            .register_state_channel::<bmux_scene_protocol::scene_protocol::DecorationScene>(
                bmux_scene_protocol::scene_protocol::STATE_KIND,
                empty_scene(),
            );
        let mut summary_theme_loaded = false;
        let mut summary_script_loaded = false;
        if let Ok(mut state) = self.state.inner.lock() {
            summary_theme_loaded = state.current_theme.is_some();
            summary_script_loaded = state.script_backend.is_some();
            // Bump the scene revision so the first build_scene() call
            // returns a non-zero revision, signalling consumers that
            // the plugin has published at least once. Emission runs
            // inside `bump_revision`, so subscribers see the initial
            // scene on their next poll.
            bump_revision(&mut state);
        }
        tracing::debug!(
            theme_loaded = summary_theme_loaded,
            script_loaded = summary_script_loaded,
            "decoration plugin activate complete",
        );
        // Spawn the windows-plugin pane-event broadcast subscriber.
        // This captures transient focus-change / zoom / lifecycle
        // events emitted by the windows plugin's focus-pane shim.
        // Activation-order races can make this subscriber miss the
        // initial focus event; the state-channel subscriber below
        // covers that gap with `subscribe_state` semantics (new
        // subscribers receive the current value immediately).
        spawn_windows_pane_event_subscriber(self.state.clone_arc());
        // Spawn the pane-runtime focus-state subscriber. Unlike the
        // broadcast subscriber above, this one is race-free: the
        // event bus replays the most recently published
        // `SessionFocusStateMap` to late subscribers before any live
        // updates arrive.
        spawn_pane_runtime_focus_state_subscriber(self.state.clone_arc());
        // Register the attach-layout state channel with a JSON
        // decoder so the attach runtime can relay layout snapshots
        // across the client/server boundary via
        // `Request::EmitOnPluginBus`. The decorator plugin lives in
        // the server process and relies on this to observe pane
        // geometry without any client-side hardcoded push helper.
        let _ = bmux_plugin::global_event_bus()
            .register_state_channel_with_decoder::<
                bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot,
            >(
                bmux_attach_layout_protocol::attach_layout_protocol::STATE_KIND,
                bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot {
                    surfaces: Vec::new(),
                    revision: 0,
                },
            );
        spawn_attach_layout_subscriber(self.state.clone_arc());
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

    #[allow(clippy::too_many_lines)] // route_service! naturally spans every typed op; splitting hurts readability.
    fn invoke_service(
        &mut self,
        context: bmux_plugin_sdk::NativeServiceContext,
    ) -> bmux_plugin_sdk::ServiceResponse {
        // IPC-level dispatch for every typed op declared in the
        // decoration BPDL. The server routes `Request::InvokeService`
        // here when a client (attach runtime, sibling plugin, etc.)
        // reaches the decoration plugin over the wire rather than the
        // in-process typed registry. Each arm decodes the
        // `bmux_codec`-encoded payload, runs the same logic the
        // `DecorationStateService` trait methods use against the
        // shared state, and encodes the response back.
        let state = self.state.clone_arc();
        bmux_plugin_sdk::route_service!(context, {
            "decoration-state", "pane-decoration" => |req: PaneDecorationArgs, _ctx| {
                let result = state
                    .lock()
                    .ok()
                    .map(|s| {
                        if let Some(p) = s.panes.get(&req.pane_id) {
                            return p.clone();
                        }
                        let focused = s.activity.get(&req.pane_id).is_some_and(|a| a.focused);
                        default_pane_decoration(req.pane_id, s.default_border, focused)
                    });
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(result)
            },
            "decoration-state", "default-border-style" => |_req: (), _ctx| {
                let border = state
                    .lock()
                    .map_or(BorderStyle::default(), |s| s.default_border);
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(border)
            },
            "decoration-state", "scene-snapshot" => |_req: (), _ctx| {
                let scene = state
                    .lock()
                    .map_or_else(|_| empty_scene(), |mut s| build_scene(&mut s));
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(scene)
            },
            "decoration-state", "pane-geometry" => |req: PaneGeometryArgs, _ctx| {
                let geom = state.lock().ok().and_then(|s| s.geometry.get(&req.pane_id).cloned());
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(geom)
            },
            "decoration-state", "pane-activity" => |req: PaneActivityArgs, _ctx| {
                let activity = state.lock().ok().and_then(|s| s.activity.get(&req.pane_id).cloned());
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(activity)
            },
            "decoration-state", "current-theme-extension" => |_req: (), _ctx| {
                let theme = state.lock().ok().and_then(|s| s.current_theme.clone());
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(theme)
            },
            "decoration-state", "validate-theme-extension" => |req: ValidateThemeExtensionArgs, _ctx| {
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(validate_theme_extension_toml(&req.toml))
            },
            "decoration-state", "set-pane-border" => |req: SetPaneBorderArgs, _ctx| {
                let outcome: Result<(), SetStyleError> = (|| {
                    let mut state = state
                        .lock()
                        .map_err(|_| SetStyleError::StyleUnsupported {
                            style: "<poisoned>".into(),
                        })?;
                    let focused = state.activity.get(&req.pane_id).is_some_and(|a| a.focused);
                    let entry = state
                        .panes
                        .entry(req.pane_id)
                        .or_insert_with(|| default_pane_decoration(req.pane_id, req.border, focused));
                    entry.border = req.border;
                    entry.focused = focused;
                    bump_revision(&mut state);
                    Ok(())
                })();
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(outcome)
            },
            "decoration-state", "set-default-border" => |req: SetDefaultBorderArgs, _ctx| {
                let outcome: Result<(), SetStyleError> = (|| {
                    let mut state = state
                        .lock()
                        .map_err(|_| SetStyleError::StyleUnsupported {
                            style: "<poisoned>".into(),
                        })?;
                    state.default_border = req.border;
                    bump_revision(&mut state);
                    Ok(())
                })();
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(outcome)
            },
            "decoration-state", "apply-theme-extension" => |req: ApplyThemeExtensionArgs, _ctx| {
                let candidates = req
                    .config_dir_candidates
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(apply_theme_extension_toml(
                    &state,
                    &req.toml,
                    &candidates,
                ))
            },
            "theme-extension", "apply" => |req: ApplyThemeExtensionArgs, _ctx| {
                let candidates = req
                    .config_dir_candidates
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(apply_theme_extension_toml(
                    &state,
                    &req.toml,
                    &candidates,
                ))
            },
            "decoration-state", "notify-pane-event" => |req: NotifyPaneEventArgs, _ctx| {
                let outcome: Result<(), NotifyError> = (|| {
                    let mut state = state
                        .lock()
                        .map_err(|_| NotifyError::InvalidArgument {
                            reason: "decoration state mutex poisoned".to_string(),
                        })?;
                    apply_pane_event(&mut state, &req.event);
                    Ok(())
                })();
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(outcome)
            },
        })
    }
}

// ── Request payload structs for `invoke_service` dispatch ───────────
//
// BPDL ops carry named parameters; `invoke_service` receives them as a
// single encoded struct. The structs below mirror the BPDL operation
// signatures exactly so `bmux_codec` round-trips cleanly against the
// client's encoded args.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PaneDecorationArgs {
    pane_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PaneGeometryArgs {
    pane_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PaneActivityArgs {
    pane_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ValidateThemeExtensionArgs {
    toml: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SetPaneBorderArgs {
    pane_id: Uuid,
    border: BorderStyle,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SetDefaultBorderArgs {
    border: BorderStyle,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ApplyThemeExtensionArgs {
    toml: String,
    config_dir_candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct NotifyPaneEventArgs {
    event: PaneEvent,
}

/// Compile `script` into a fresh backend and install it on `state`.
/// Invoked during `activate` before the first revision bump so the
/// initial published scene already reflects any script output.
///
/// Failure modes (compile error, stub backend when no `scripting-*`
/// feature is compiled in) are logged at `warn` and leave the plugin
/// in its non-scripted state — the rest of the decoration pipeline
/// keeps working.
fn install_script_backend(state: &mut State, script: Option<ResolvedScript>) {
    let Some(script) = script else {
        state.script_backend = None;
        state.script_path = None;
        state.script_source_hash = None;
        state.script_started_at = None;
        state.script_frame = 0;
        state.script_perf = None;
        state.script_events.clear();
        state.script_event_subscriptions.clear();
        state.script_first_invoke_logged = false;
        return;
    };
    let source_hash = script_source_hash(&script.path, &script.source);
    if state.script_backend.is_some()
        && state.script_path.as_ref() == Some(&script.path)
        && state.script_source_hash == Some(source_hash)
    {
        tracing::debug!(
            script = ?script.path,
            "decoration script unchanged; preserving existing backend",
        );
        return;
    }
    let backend = crate::scripting::make_backend();
    if !backend.is_functional() {
        tracing::warn!(
            target: "decoration.script",
            script = ?script.path,
            "decoration scripting is not compiled into this build — script will be ignored",
        );
        return;
    }
    if let Err(err) = backend.compile(&script.path, &script.source) {
        tracing::warn!(
            target: "decoration.script",
            script = ?script.path,
            error = %err,
            "decoration script failed to compile — falling back to static decorations",
        );
        return;
    }
    state.script_backend = Some(backend);
    state.script_path = Some(script.path.clone());
    state.script_source_hash = Some(source_hash);
    state.script_started_at = Some(Instant::now());
    state.script_frame = 0;
    state.script_perf = Some(PerfTracker::new(
        script.path.clone(),
        crate::scripting::DEFAULT_WARN_MS,
    ));
    state.script_events.clear();
    state.script_event_subscriptions.clear();
    tracing::debug!(
        script = ?script.path,
        backend = state
            .script_backend
            .as_ref()
            .map_or("none", |b| b.name()),
        "decoration script compiled and installed",
    );
}

fn script_source_hash(path: &std::path::Path, source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}

/// Background timer that re-invokes the decoration script at `hz`
/// ticks per second while the plugin's shared state is alive. The
/// thread holds a [`Weak`] reference so it terminates cleanly when
/// the plugin (and thus the `Arc<Mutex<State>>`) is dropped.
fn spawn_animation_tick_thread(state: Weak<Mutex<State>>, hz: u16) {
    // `u16` hz * `Duration::from_micros` keeps arithmetic safe up to
    // 65535 Hz. We do not clamp — users are responsible for the CPU
    // cost of their chosen frame rate.
    let period = Duration::from_micros((1_000_000u64 / u64::from(hz.max(1))).max(1));
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(period);
            let Some(arc) = state.upgrade() else {
                return;
            };
            let Ok(mut guard) = arc.lock() else {
                return;
            };
            if guard.animation_hz != Some(hz) {
                return;
            }
            // Skip the tick entirely if the script was unloaded
            // between frames — avoids a useless revision bump.
            if guard.script_backend.is_some() {
                bump_revision(&mut guard);
            }
        }
    });
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

/// Subscribe to the pane-runtime focus-state channel and keep
/// `state.activity` in sync with the authoritative per-session focus
/// snapshot.
///
/// The state channel (registered by the pane-runtime plugin via
/// [`EventBus::register_state_channel`]) retains the last-published
/// `SessionFocusStateMap` and replays it synchronously to any new
/// subscriber. This closes the late-subscriber gap that plain
/// broadcast channels can't — regardless of whether the decoration
/// plugin activates before or after pane-runtime, it observes the
/// current focus state.
///
/// The subscriber thread reconciles the full map against
/// `state.activity` on every update: every pane listed as
/// `focused_pane_id` gets `activity.focused = true`, every other
/// known pane gets `focused = false`. A fresh revision bumps the
/// scene so downstream consumers (attach renderer) pick up the
/// change.
fn spawn_pane_runtime_focus_state_subscriber(state: Arc<Mutex<State>>) {
    let subscribe_result = bmux_plugin::global_event_bus()
        .subscribe_state::<bmux_pane_runtime_plugin_api::pane_runtime_focus::SessionFocusStateMap>(
        &bmux_pane_runtime_plugin_api::pane_runtime_focus::STATE_KIND,
    );
    let (initial, mut rx) = match subscribe_result {
        Ok(pair) => {
            tracing::debug!("focus-state subscribe OK");
            pair
        }
        Err(err) => {
            tracing::warn!(%err, "focus-state subscribe FAILED");
            return;
        }
    };
    // Apply the initial snapshot immediately so scripts see the correct focus
    // before their first render message.
    tracing::debug!(
        entries = initial.entries.len(),
        revision = initial.revision,
        "focus-state initial applied"
    );
    if let Ok(mut guard) = state.lock() {
        apply_focus_state_map(&mut guard, initial.as_ref());
    }
    std::thread::spawn(move || {
        // Drive the watch receiver on a dedicated tokio runtime — we
        // don't have an ambient runtime at this call site and the
        // thread is plugin-lifetime long anyway, so the one-time
        // construction cost is acceptable.
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::error!("focus-state subscriber thread FAILED to build tokio runtime");
            return;
        };
        rt.block_on(async move {
            while rx.changed().await.is_ok() {
                let snapshot = rx.borrow().clone();
                tracing::trace!(
                    entries = snapshot.entries.len(),
                    revision = snapshot.revision,
                    "focus-state update"
                );
                let Ok(mut guard) = state.lock() else {
                    break;
                };
                apply_focus_state_map(&mut guard, snapshot.as_ref());
            }
            tracing::debug!("focus-state subscriber loop exited");
        });
    });
}

/// Reconcile the decoration plugin's `state.activity` map against a
/// pane-runtime focus snapshot. Any pane listed as a focused pane in
/// the snapshot is marked `focused = true`; all other known panes are
/// unfocused. The scene revision bumps when anything changes.
fn apply_focus_state_map(
    state: &mut State,
    snapshot: &bmux_pane_runtime_plugin_api::pane_runtime_focus::SessionFocusStateMap,
) {
    use std::collections::BTreeSet;
    let focused: BTreeSet<Uuid> = snapshot
        .entries
        .values()
        .map(|entry| entry.focused_pane_id)
        .collect();
    let mut changed = false;
    // Unfocus everything not in the focused set.
    for (pane_id, act) in &mut state.activity {
        let should_focus = focused.contains(pane_id);
        if act.focused != should_focus {
            act.focused = should_focus;
            changed = true;
        }
    }
    // Ensure focused panes we haven't seen before exist in the
    // activity map with `focused = true`.
    let snapshot_focused_count = focused.len();
    for pane_id in focused {
        let needs_insert = !state.activity.contains_key(&pane_id);
        if needs_insert {
            let entry = state.activity_mut(pane_id);
            entry.focused = true;
            state.sync_focused_mirror(pane_id, true);
            changed = true;
        } else {
            // If the pane already existed but was just flipped above,
            // mirror the focused bit onto the pane's decoration row.
            state.sync_focused_mirror(pane_id, true);
        }
    }
    let activity_focused_after = state.activity.values().filter(|a| a.focused).count();
    let activity_total = state.activity.len();
    tracing::trace!(
        snapshot_focused = snapshot_focused_count,
        changed,
        activity_total,
        activity_focused_after,
        "apply_focus_state_map"
    );
    if changed {
        bump_revision(state);
    }
}

/// Subscribe to the attach-layout state channel and reconcile
/// incoming snapshots into `state.geometry`. Each snapshot carries
/// the set of visible attach surfaces; we insert/update the
/// `PaneGeometry` for every pane-backed surface and drop any panes
/// that disappeared from the new snapshot. Bumping the scene
/// revision after a change lets subscribers pick up the updated
/// paint commands on the next frame.
fn spawn_attach_layout_subscriber(state: Arc<Mutex<State>>) {
    let subscribe_result = bmux_plugin::global_event_bus()
        .subscribe_state::<
            bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot,
        >(
        &bmux_attach_layout_protocol::attach_layout_protocol::STATE_KIND,
    );
    let (initial, mut rx) = match subscribe_result {
        Ok(pair) => {
            tracing::debug!("attach-layout subscribe OK");
            pair
        }
        Err(err) => {
            tracing::warn!(%err, "attach-layout subscribe FAILED");
            return;
        }
    };
    if let Ok(mut guard) = state.lock() {
        apply_attach_layout_snapshot(&mut guard, initial.as_ref());
    }
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::error!("attach-layout subscriber FAILED to build tokio runtime");
            return;
        };
        rt.block_on(async move {
            while rx.changed().await.is_ok() {
                let snapshot = rx.borrow().clone();
                tracing::trace!(
                    surfaces = snapshot.surfaces.len(),
                    revision = snapshot.revision,
                    "attach-layout update"
                );
                let Ok(mut guard) = state.lock() else {
                    break;
                };
                apply_attach_layout_snapshot(&mut guard, snapshot.as_ref());
            }
            tracing::debug!("attach-layout subscriber loop exited");
        });
    });
}

/// Reconcile `state.geometry` against an [`AttachLayoutSnapshot`].
/// Surfaces backed by a pane (non-`None` `pane_id`) update the
/// plugin's geometry record; panes that disappeared from the
/// snapshot are removed. `state.activity` entries for removed panes
/// are cleaned up too so stale focus / zoom flags don't linger.
fn apply_attach_layout_snapshot(
    state: &mut State,
    snapshot: &bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot,
) {
    use std::collections::BTreeSet;
    let mut changed = false;
    let mut seen: BTreeSet<Uuid> = BTreeSet::new();
    for surface in &snapshot.surfaces {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        seen.insert(pane_id);
        let new_geometry = PaneGeometry {
            pane_id,
            rect: surface.rect.clone(),
            content_rect: surface.content_rect.clone(),
        };
        let prev = state.geometry.insert(pane_id, new_geometry);
        if prev.as_ref() != state.geometry.get(&pane_id) {
            changed = true;
        }
    }
    // Drop panes that are no longer in the visible set.
    let drop_ids: Vec<Uuid> = state
        .geometry
        .keys()
        .filter(|id| !seen.contains(id))
        .copied()
        .collect();
    for pane_id in drop_ids {
        state.geometry.remove(&pane_id);
        state.activity.remove(&pane_id);
        state.panes.remove(&pane_id);
        changed = true;
    }
    if changed {
        bump_revision(state);
    }
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
    use std::path::Path;

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

    // Helpers shared by the new theme-aware build-scene tests below.
    // Seeding geometry and activity directly on the shared state lets
    // each test exercise `build_scene` without routing through the
    // full IPC path.
    fn seed_geometry(plugin: &DecorationPlugin, pane: Uuid, w: u16, h: u16) {
        if let Ok(mut state) = plugin.state.inner.lock() {
            state.geometry.insert(
                pane,
                PaneGeometry {
                    pane_id: pane,
                    rect: Rect { x: 0, y: 0, w, h },
                    content_rect: Rect {
                        x: 1,
                        y: 1,
                        w: w.saturating_sub(2),
                        h: h.saturating_sub(2),
                    },
                },
            );
        }
    }

    fn set_activity(plugin: &DecorationPlugin, pane: Uuid, focused: bool, zoomed: bool) {
        if let Ok(mut state) = plugin.state.inner.lock() {
            let entry = state.activity.entry(pane).or_insert(PaneActivity {
                pane_id: pane,
                focused: false,
                zoomed: false,
                status: PaneLifecycle::Running,
            });
            entry.focused = focused;
            entry.zoomed = zoomed;
        }
    }

    fn sample_theme() -> DecorationThemeExtension {
        DecorationThemeExtension {
            unfocused: BorderSpec {
                style: "single-line".to_string(),
                fg: "#1a4d1a".to_string(),
                bg: String::new(),
                gradient_from: String::new(),
                gradient_to: String::new(),
                gradient_axis: String::new(),
                glyphs_custom: Vec::new(),
            },
            focused: BorderSpec {
                style: "thick".to_string(),
                fg: "#39ff14".to_string(),
                bg: String::new(),
                gradient_from: String::new(),
                gradient_to: String::new(),
                gradient_axis: String::new(),
                glyphs_custom: Vec::new(),
            },
            zoomed: BorderSpec {
                style: "double".to_string(),
                fg: "#ffd700".to_string(),
                bg: String::new(),
                gradient_from: String::new(),
                gradient_to: String::new(),
                gradient_axis: String::new(),
                glyphs_custom: Vec::new(),
            },
            badges: bmux_decoration_plugin_api::decoration_state::BadgeSpec {
                running: String::new(),
                exited: String::new(),
            },
            animation: None,
            script: None,
        }
    }

    fn install_theme(plugin: &DecorationPlugin, theme: DecorationThemeExtension) {
        if let Ok(mut state) = plugin.state.inner.lock() {
            state.current_theme = Some(theme);
        }
    }

    fn decoration_extension_from_theme(theme: &str) -> DecorationThemeExtension {
        let parsed = toml::from_str::<toml::Value>(theme).expect("theme TOML parses");
        parsed
            .get("plugins")
            .and_then(|plugins| plugins.get("bmux.decoration"))
            .expect("theme contains bmux.decoration plugin slice")
            .clone()
            .try_into()
            .expect("bmux.decoration plugin slice matches schema")
    }

    fn install_extension_with_script(
        plugin: &DecorationPlugin,
        extension: DecorationThemeExtension,
    ) {
        let script = extension
            .script
            .as_deref()
            .and_then(|spec| resolve_decoration_script(&[], spec));
        let mut state = plugin.state.inner.lock().expect("lock");
        state.animation_hz = extension.animation.as_ref().map(|animation| animation.hz);
        state.current_theme = Some(extension);
        install_script_backend(&mut state, script);
    }

    fn box_border_of(scene: &DecorationScene, pane: &Uuid) -> PaintCommand {
        let surface = scene
            .surfaces
            .get(pane)
            .expect("surface should exist for seeded pane");
        surface
            .paint_commands
            .iter()
            .find(|c| matches!(c, PaintCommand::BoxBorder { .. }))
            .cloned()
            .expect("surface must carry a BoxBorder paint command")
    }

    #[test]
    fn build_scene_emits_themed_unfocused_border() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa1);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, false, false);
        install_theme(&plugin, sample_theme());
        let scene = plugin.build_scene();
        let PaintCommand::BoxBorder { glyphs, style, .. } = box_border_of(&scene, &pane) else {
            panic!("expected BoxBorder");
        };
        assert_eq!(glyphs, BorderGlyphs::SingleLine);
        assert_eq!(
            style.fg,
            Some(Color::Rgb {
                r: 0x1a,
                g: 0x4d,
                b: 0x1a,
            })
        );
    }

    #[test]
    fn build_scene_emits_themed_focused_border() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa2);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, false);
        install_theme(&plugin, sample_theme());
        let scene = plugin.build_scene();
        let PaintCommand::BoxBorder { glyphs, style, .. } = box_border_of(&scene, &pane) else {
            panic!("expected BoxBorder");
        };
        assert_eq!(glyphs, BorderGlyphs::Thick);
        assert_eq!(
            style.fg,
            Some(Color::Rgb {
                r: 0x39,
                g: 0xff,
                b: 0x14,
            })
        );
    }

    #[test]
    fn build_scene_emits_themed_zoomed_border() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa3);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, true);
        install_theme(&plugin, sample_theme());
        let scene = plugin.build_scene();
        let PaintCommand::BoxBorder { glyphs, style, .. } = box_border_of(&scene, &pane) else {
            panic!("expected BoxBorder");
        };
        assert_eq!(glyphs, BorderGlyphs::DoubleLine);
        assert_eq!(
            style.fg,
            Some(Color::Rgb {
                r: 0xff,
                g: 0xd7,
                b: 0x00,
            })
        );
    }

    #[test]
    fn build_scene_falls_back_to_rounded_when_theme_absent() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa4);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, false, false);
        let scene = plugin.build_scene();
        let PaintCommand::BoxBorder { glyphs, style, .. } = box_border_of(&scene, &pane) else {
            panic!("expected BoxBorder");
        };
        assert_eq!(glyphs, BorderGlyphs::Rounded);
        assert_eq!(
            style.fg,
            Some(Color::Named {
                name: NamedColor::White,
            })
        );
        assert!(!style.bold);
    }

    #[test]
    fn build_scene_default_focused_is_bold_bright_white() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa5);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, false);
        let scene = plugin.build_scene();
        let PaintCommand::BoxBorder { glyphs, style, .. } = box_border_of(&scene, &pane) else {
            panic!("expected BoxBorder");
        };
        assert_eq!(glyphs, BorderGlyphs::Rounded);
        assert_eq!(
            style.fg,
            Some(Color::Named {
                name: NamedColor::BrightWhite,
            })
        );
        assert!(style.bold);
    }

    #[test]
    fn build_scene_override_wins_over_theme() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(0xa6);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, false, false);
        install_theme(&plugin, sample_theme());
        // Explicit override: user chose Double. This must win even
        // though the theme's unfocused spec is SingleLine.
        block_on(handle.set_pane_border(pane, BorderStyle::Double)).expect("set");
        let scene = plugin.build_scene();
        let PaintCommand::BoxBorder { glyphs, .. } = box_border_of(&scene, &pane) else {
            panic!("expected BoxBorder");
        };
        assert_eq!(glyphs, BorderGlyphs::DoubleLine);
    }

    #[test]
    fn build_scene_horizontal_gradient_emits_four_gradient_runs() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa7);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, false);
        let mut theme = sample_theme();
        theme.focused.gradient_from = "#ff0000".to_string();
        theme.focused.gradient_to = "#0000ff".to_string();
        theme.focused.gradient_axis = "horizontal".to_string();
        install_theme(&plugin, theme);
        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface present");
        let gradients: Vec<_> = surface
            .paint_commands
            .iter()
            .filter(|c| matches!(c, PaintCommand::GradientRun { .. }))
            .collect();
        assert_eq!(
            gradients.len(),
            4,
            "horizontal gradient emits top/bottom/left/right runs"
        );
    }

    #[test]
    fn build_scene_vertical_gradient_emits_four_gradient_runs() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa8);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, false);
        let mut theme = sample_theme();
        theme.focused.gradient_from = "#00ff00".to_string();
        theme.focused.gradient_to = "#ff00ff".to_string();
        theme.focused.gradient_axis = "vertical".to_string();
        install_theme(&plugin, theme);
        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface present");
        let gradients: Vec<_> = surface
            .paint_commands
            .iter()
            .filter(|c| matches!(c, PaintCommand::GradientRun { .. }))
            .collect();
        assert_eq!(gradients.len(), 4);
    }

    #[test]
    fn build_scene_diagonal_gradient_emits_cell_grids() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa9);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, false);
        let mut theme = sample_theme();
        theme.focused.gradient_from = "#ff0000".to_string();
        theme.focused.gradient_to = "#0000ff".to_string();
        theme.focused.gradient_axis = "diagonal".to_string();
        install_theme(&plugin, theme);
        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface present");
        let cell_grids: Vec<_> = surface
            .paint_commands
            .iter()
            .filter(|c| matches!(c, PaintCommand::CellGrid { .. }))
            .collect();
        // Top + bottom + left + right = 4 CellGrids (left+right only
        // emitted when height > 2).
        assert_eq!(cell_grids.len(), 4);
    }

    #[test]
    fn parse_hex_color_handles_valid_and_invalid_inputs() {
        assert_eq!(
            parse_hex_color("#39ff14"),
            Some(Color::Rgb {
                r: 0x39,
                g: 0xff,
                b: 0x14,
            })
        );
        assert_eq!(parse_hex_color("39ff14"), None);
        assert_eq!(parse_hex_color("#xyz000"), None);
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#fff"), None);
    }

    #[test]
    fn parse_gradient_axis_accepts_kebab_and_snake() {
        assert_eq!(parse_gradient_axis("horizontal"), GradientAxis::Horizontal);
        assert_eq!(parse_gradient_axis("Vertical"), GradientAxis::Vertical);
        assert_eq!(parse_gradient_axis("diagonal"), GradientAxis::Diagonal);
        assert_eq!(parse_gradient_axis(""), GradientAxis::Horizontal);
        assert_eq!(parse_gradient_axis("unknown"), GradientAxis::Horizontal);
    }

    #[test]
    fn setting_pane_border_bumps_revision_and_populates_scene() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(42);
        // Seed geometry first — `build_scene` now uses `state.geometry`
        // as the authoritative set of visible panes. Setting an
        // override via `set-pane-border` only affects paint-command
        // selection for panes that also have geometry reported to the
        // plugin.
        if let Ok(mut state) = plugin.state.inner.lock() {
            state.geometry.insert(
                pane,
                PaneGeometry {
                    pane_id: pane,
                    rect: Rect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 5,
                    },
                    content_rect: Rect {
                        x: 1,
                        y: 1,
                        w: 18,
                        h: 3,
                    },
                },
            );
        }
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");
        let scene = plugin.build_scene();
        assert!(scene.revision >= 1);
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
            config_dir_candidates: vec!["/tmp".to_string()],
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
    fn apply_attach_layout_snapshot_caches_rects_and_bumps_revision() {
        use bmux_attach_layout_protocol::attach_layout_protocol::{
            AttachLayoutSnapshot, AttachSurfaceSummary,
        };
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(100);
        let before = plugin.build_scene().revision;
        let snapshot = AttachLayoutSnapshot {
            surfaces: vec![AttachSurfaceSummary {
                surface_id: pane,
                pane_id: Some(pane),
                rect: rect(0, 0, 20, 5),
                content_rect: rect(1, 1, 18, 3),
                visible: true,
            }],
            revision: 1,
        };
        {
            let mut state = plugin.state.inner.lock().expect("state");
            apply_attach_layout_snapshot(&mut state, &snapshot);
        }
        let after = plugin.build_scene().revision;
        assert!(after > before);
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let geom = block_on(handle.pane_geometry(pane)).expect("geometry cached");
        assert_eq!(geom.rect, rect(0, 0, 20, 5));
        assert_eq!(geom.content_rect, rect(1, 1, 18, 3));
    }

    #[test]
    fn apply_attach_layout_snapshot_skips_revision_bump_for_unchanged_rects() {
        use bmux_attach_layout_protocol::attach_layout_protocol::{
            AttachLayoutSnapshot, AttachSurfaceSummary,
        };
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(101);
        let snapshot = AttachLayoutSnapshot {
            surfaces: vec![AttachSurfaceSummary {
                surface_id: pane,
                pane_id: Some(pane),
                rect: rect(0, 0, 10, 5),
                content_rect: rect(1, 1, 8, 3),
                visible: true,
            }],
            revision: 1,
        };
        {
            let mut state = plugin.state.inner.lock().expect("state");
            apply_attach_layout_snapshot(&mut state, &snapshot);
        }
        let r1 = plugin.build_scene().revision;
        {
            let mut state = plugin.state.inner.lock().expect("state");
            apply_attach_layout_snapshot(&mut state, &snapshot);
        }
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
    fn dropping_pane_from_attach_layout_clears_all_state() {
        use bmux_attach_layout_protocol::attach_layout_protocol::{
            AttachLayoutSnapshot, AttachSurfaceSummary,
        };
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(600);
        block_on(handle.set_pane_border(pane, BorderStyle::Double)).expect("set");
        let snapshot = AttachLayoutSnapshot {
            surfaces: vec![AttachSurfaceSummary {
                surface_id: pane,
                pane_id: Some(pane),
                rect: rect(0, 0, 10, 5),
                content_rect: rect(1, 1, 8, 3),
                visible: true,
            }],
            revision: 1,
        };
        {
            let mut state = plugin.state.inner.lock().expect("state");
            apply_attach_layout_snapshot(&mut state, &snapshot);
        }
        block_on(handle.notify_pane_event(PaneEvent::Focused { pane_id: pane })).expect("focus");
        // Empty snapshot — pane disappears from the attach layout and
        // the decoration plugin drops all state for it.
        let empty = AttachLayoutSnapshot {
            surfaces: Vec::new(),
            revision: 2,
        };
        {
            let mut state = plugin.state.inner.lock().expect("state");
            apply_attach_layout_snapshot(&mut state, &empty);
        }
        assert!(block_on(handle.pane_geometry(pane)).is_none());
        assert!(block_on(handle.pane_activity(pane)).is_none());
        let deco = block_on(handle.pane_decoration(pane)).expect("default");
        assert!(!deco.focused);
    }

    #[test]
    fn build_scene_includes_geometry_when_pane_has_override_and_layout() {
        use bmux_attach_layout_protocol::attach_layout_protocol::{
            AttachLayoutSnapshot, AttachSurfaceSummary,
        };
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(700);
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");
        let snapshot = AttachLayoutSnapshot {
            surfaces: vec![AttachSurfaceSummary {
                surface_id: pane,
                pane_id: Some(pane),
                rect: rect(2, 3, 20, 5),
                content_rect: rect(3, 4, 18, 3),
                visible: true,
            }],
            revision: 1,
        };
        {
            let mut state = plugin.state.inner.lock().expect("state");
            apply_attach_layout_snapshot(&mut state, &snapshot);
        }
        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface present");
        assert_eq!(surface.rect, rect(2, 3, 20, 5));
        assert_eq!(surface.content_rect, rect(3, 4, 18, 3));
    }

    // PR 3: scene-state publication. The decoration plugin publishes a
    // retained `DecorationScene` on the typed event bus every time state
    // mutates; late attach clients hydrate from the current value.
    #[test]
    fn bump_revision_publishes_retained_scene_when_channel_registered() {
        // Register the state channel first (as `activate()` would).
        let _sender = bmux_plugin::global_event_bus()
            .register_state_channel::<bmux_scene_protocol::scene_protocol::DecorationScene>(
            bmux_scene_protocol::scene_protocol::STATE_KIND,
            empty_scene(),
        );

        let (_initial, mut rx) = bmux_plugin::global_event_bus()
            .subscribe_state::<bmux_scene_protocol::scene_protocol::DecorationScene>(
                &bmux_scene_protocol::scene_protocol::STATE_KIND,
            )
            .expect("subscribe");

        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        let pane = Uuid::from_u128(900);
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");

        assert!(rx.has_changed().expect("state channel open"));
        let scene = rx.borrow_and_update().clone();
        assert!(scene.revision >= 1);
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
    fn current_theme_extension_returns_none_on_fresh_plugin() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone_arc());
        assert!(block_on(handle.current_theme_extension()).is_none());
    }

    // ─── PR 5: Luau scripting ─────────────────────────────────────

    #[test]
    fn theme_without_script_field_still_parses() {
        let extension: DecorationThemeExtension = toml::from_str::<toml::Value>(
            r##"
[unfocused]
style = "rounded"
fg = "#606060"
bg = ""
gradient_from = ""
gradient_to = ""
glyphs_custom = []

[focused]
style = "thick"
fg = "#e0e0e0"
bg = ""
gradient_from = ""
gradient_to = ""
glyphs_custom = []

[zoomed]
style = "double"
fg = "#ffffff"
bg = ""
gradient_from = ""
gradient_to = ""
glyphs_custom = []

[badges]
running = ""
exited = ""
"##,
        )
        .expect("parse extension")
        .try_into()
        .expect("extension parses");
        assert!(
            extension.script.is_none(),
            "extension does not declare a script"
        );
    }

    #[test]
    fn resolve_decoration_script_matches_bundled_name() {
        let tmp = std::env::temp_dir().join(format!("bmux-script-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let resolved = resolve_decoration_script(std::slice::from_ref(&tmp), "pulse")
            .expect("bundled `pulse` script must resolve by bare name");
        assert!(
            resolved.source.contains("function decorate"),
            "resolved pulse source must contain a decorate function"
        );
        assert_eq!(resolved.path.to_str(), Some("bundled:pulse"));
    }

    #[test]
    fn resolve_decoration_script_reads_filesystem_path() {
        let tmp = std::env::temp_dir().join(format!("bmux-script-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(tmp.join("decorations")).expect("mkdir");
        let rel = "decorations/test.lua";
        let body = "function decorate(message) return {} end\n";
        std::fs::write(tmp.join(rel), body).expect("write script");
        let resolved = resolve_decoration_script(std::slice::from_ref(&tmp), rel)
            .expect("filesystem script must resolve against config_dir");
        assert_eq!(resolved.source, body);
        assert!(resolved.path.ends_with(rel));
    }

    #[test]
    fn resolve_decoration_script_returns_none_for_unknown_name() {
        let tmp = std::env::temp_dir().join(format!("bmux-script-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        assert!(resolve_decoration_script(std::slice::from_ref(&tmp), "no-such-script").is_none());
    }

    #[test]
    fn install_script_backend_compiles_and_stores_backend() {
        let plugin = DecorationPlugin::new();
        {
            let mut state = plugin.state.inner.lock().expect("lock");
            install_script_backend(
                &mut state,
                Some(ResolvedScript {
                    path: PathBuf::from("bundled:test"),
                    source: "function decorate(message) return {} end".into(),
                }),
            );
            assert!(
                state.script_backend.is_some(),
                "backend must be installed after a successful compile"
            );
            assert_eq!(
                state.script_path.as_deref(),
                Some(Path::new("bundled:test"))
            );
            assert!(state.script_started_at.is_some());
            assert!(state.script_source_hash.is_some());
        }
    }

    #[test]
    fn install_script_backend_preserves_identical_script_backend() {
        let plugin = DecorationPlugin::new();
        let mut state = plugin.state.inner.lock().expect("lock");
        let script = ResolvedScript {
            path: PathBuf::from("bundled:test"),
            source: "function decorate(message) return {} end".into(),
        };
        install_script_backend(&mut state, Some(script.clone()));
        let started_at = state
            .script_started_at
            .expect("initial install records start instant");
        let source_hash = state
            .script_source_hash
            .expect("initial install records source hash");
        state.script_frame = 42;

        install_script_backend(&mut state, Some(script));

        assert_eq!(state.script_started_at, Some(started_at));
        assert_eq!(state.script_source_hash, Some(source_hash));
        assert_eq!(state.script_frame, 42, "frame is preserved");
    }

    #[test]
    fn install_script_backend_discards_on_compile_failure() {
        let plugin = DecorationPlugin::new();
        let mut state = plugin.state.inner.lock().expect("lock");
        install_script_backend(
            &mut state,
            Some(ResolvedScript {
                path: PathBuf::from("bundled:broken"),
                source: "function decorate(ctx return {}".into(),
            }),
        );
        assert!(
            state.script_backend.is_none(),
            "compile failure must leave backend unset"
        );
    }

    #[test]
    fn build_scene_merges_script_paint_commands_for_known_geometry() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(777);
        {
            let mut state = plugin.state.inner.lock().expect("lock");
            install_script_backend(
                &mut state,
                Some(ResolvedScript {
                    path: PathBuf::from("bundled:test"),
                    source: r#"
                        function decorate(message)
                            if message.kind ~= "render" then
                                return nil
                            end
                            local surfaces = {}
                            for _, pane in ipairs(message.panes or {}) do
                                surfaces[pane.id] = {
                                    {
                                        kind = "text",
                                        col = pane.rect.x,
                                        row = pane.rect.y,
                                        z = 5,
                                        text = "hi",
                                        style = {},
                                    },
                                }
                            end
                            return { surfaces = surfaces }
                        end
                    "#
                    .into(),
                }),
            );
            state.geometry.insert(
                pane,
                PaneGeometry {
                    pane_id: pane,
                    rect: SceneRect {
                        x: 3,
                        y: 4,
                        w: 10,
                        h: 2,
                    },
                    content_rect: SceneRect {
                        x: 4,
                        y: 5,
                        w: 8,
                        h: 0,
                    },
                },
            );
        }
        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface emitted");
        let text_cmd = surface
            .paint_commands
            .iter()
            .find_map(|cmd| match cmd {
                PaintCommand::Text { col, row, text, .. } => Some((*col, *row, text.clone())),
                _ => None,
            })
            .expect("script's text paint command must appear in the scene");
        assert_eq!(text_cmd, (3, 4, "hi".to_string()));
    }

    #[test]
    fn pulse_demo_theme_slice_installs_and_runs_bundled_script() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xf001);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, false);

        let theme = include_str!("../../theme-plugin/assets/themes/pulse-demo.toml");
        let extension = decoration_extension_from_theme(theme);
        install_extension_with_script(&plugin, extension);

        {
            let state = plugin.state.inner.lock().expect("lock");
            assert!(state.script_backend.is_some(), "script backend installed");
            assert_eq!(
                state.script_path.as_deref(),
                Some(Path::new("bundled:pulse"))
            );
        }

        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface emitted");
        let has_script_border = surface.paint_commands.iter().any(|cmd| {
            matches!(cmd, PaintCommand::BoxBorder { z: 10, glyphs, .. } if *glyphs == BorderGlyphs::Thick)
        });
        assert!(has_script_border, "pulse script border command emitted");
    }

    #[test]
    fn rainbow_snake_theme_slice_installs_and_runs_bundled_script() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xf002);
        seed_geometry(&plugin, pane, 20, 5);
        set_activity(&plugin, pane, true, false);

        let theme = include_str!("../../theme-plugin/assets/themes/rainbow-snake.toml");
        let extension = decoration_extension_from_theme(theme);
        install_extension_with_script(&plugin, extension);

        {
            let state = plugin.state.inner.lock().expect("lock");
            assert!(state.script_backend.is_some(), "script backend installed");
            assert_eq!(
                state.script_path.as_deref(),
                Some(Path::new("bundled:rainbow_snake")),
            );
        }

        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface emitted");
        let has_snake_text = surface
            .paint_commands
            .iter()
            .any(|cmd| matches!(cmd, PaintCommand::Text { z: 20, text, .. } if text == "◆"));
        assert!(has_snake_text, "rainbow snake text command emitted");
    }

    #[test]
    fn tick_thread_exits_cleanly_when_plugin_is_dropped() {
        let plugin = DecorationPlugin::new();
        let weak = Arc::downgrade(&plugin.state.inner);
        spawn_animation_tick_thread(weak.clone(), 100);
        drop(plugin);
        // After the strong arc is dropped, the Weak upgrade must
        // fail; the thread either already exited or is blocked in
        // sleep and will exit on the next iteration. Give it a
        // moment and confirm the weak count drops to zero.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            weak.strong_count(),
            0,
            "plugin state must be fully released after drop",
        );
    }

    #[test]
    fn script_backend_not_installed_when_theme_has_no_script() {
        let plugin = DecorationPlugin::new();
        let mut state = plugin.state.inner.lock().expect("lock");
        install_script_backend(&mut state, None);
        assert!(
            state.script_backend.is_none(),
            "install_script_backend must leave no backend when script is None",
        );
    }

    #[test]
    fn install_script_backend_none_clears_existing_backend() {
        let plugin = DecorationPlugin::new();
        let mut state = plugin.state.inner.lock().expect("lock");
        install_script_backend(
            &mut state,
            Some(ResolvedScript {
                path: PathBuf::from("bundled:test"),
                source: "function decorate(message) return {} end".into(),
            }),
        );
        assert!(state.script_backend.is_some(), "backend installed first");

        install_script_backend(&mut state, None);

        assert!(state.script_backend.is_none(), "backend cleared");
        assert!(state.script_path.is_none(), "script path cleared");
        assert!(state.script_source_hash.is_none(), "source hash cleared");
        assert!(state.script_started_at.is_none(), "start instant cleared");
        assert!(state.script_perf.is_none(), "perf tracker cleared");
        assert_eq!(state.script_frame, 0, "frame reset");
    }

    #[test]
    fn resolve_decoration_script_probes_all_candidates_for_filesystem_paths() {
        let base = std::env::temp_dir().join(format!("bmux-chain-test-{}", Uuid::new_v4()));
        let primary = base.join("primary");
        let secondary = base.join("secondary");
        std::fs::create_dir_all(&primary).expect("mkdir primary");
        std::fs::create_dir_all(secondary.join("decorations"))
            .expect("mkdir secondary decorations");
        let body = "function decorate(message) return {} end\n";
        std::fs::write(secondary.join("decorations/custom.lua"), body).expect("write script");

        // Primary dir lacks the script; secondary has it.
        let resolved = resolve_decoration_script(
            &[primary.clone(), secondary.clone()],
            "decorations/custom.lua",
        )
        .expect("loader must succeed using the secondary candidate");
        assert_eq!(resolved.source, body);
        assert!(resolved.path.starts_with(&secondary));
    }

    #[test]
    fn probe_config_file_falls_back_to_config_dir_when_chain_is_empty() {
        use bmux_plugin_sdk::HostConnectionInfo;
        let tmp = std::env::temp_dir().join(format!("bmux-probe-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        std::fs::write(tmp.join("bmux.toml"), "").expect("write");

        let info = HostConnectionInfo {
            config_dir: tmp.to_string_lossy().into_owned(),
            config_dir_candidates: Vec::new(),
            runtime_dir: "/tmp".to_string(),
            data_dir: "/tmp".to_string(),
            state_dir: "/tmp".to_string(),
        };
        let probed = info
            .probe_config_file("bmux.toml")
            .expect("probe must fall back to config_dir when chain is empty");
        assert!(probed.ends_with("bmux.toml"));
    }

    #[test]
    fn probe_config_file_uses_chain_when_populated() {
        use bmux_plugin_sdk::HostConnectionInfo;
        let base = std::env::temp_dir().join(format!("bmux-probe-test-{}", Uuid::new_v4()));
        let primary = base.join("primary");
        let secondary = base.join("secondary");
        std::fs::create_dir_all(&primary).expect("mkdir primary");
        std::fs::create_dir_all(&secondary).expect("mkdir secondary");
        std::fs::write(secondary.join("bmux.toml"), "").expect("write secondary");

        let info = HostConnectionInfo {
            config_dir: primary.to_string_lossy().into_owned(),
            config_dir_candidates: vec![
                primary.to_string_lossy().into_owned(),
                secondary.to_string_lossy().into_owned(),
            ],
            runtime_dir: "/tmp".to_string(),
            data_dir: "/tmp".to_string(),
            state_dir: "/tmp".to_string(),
        };
        let probed = info
            .probe_config_file("bmux.toml")
            .expect("probe must find the secondary candidate");
        assert!(probed.starts_with(&secondary));
    }
}
