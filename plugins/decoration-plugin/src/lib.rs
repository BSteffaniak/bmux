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

mod engine;
pub mod glyphs;
pub mod scripting;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque, hash_map::DefaultHasher};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use bmux_decoration_plugin_api::decoration_commands::{DecorationCommandsService, NotifyError};
use bmux_decoration_plugin_api::decoration_events::{DecorationEvent, PaneEvent};
use bmux_decoration_plugin_api::decoration_state::{
    BorderSpec, BorderStyle, DecorationComponentSpec, DecorationComponentTargetSpec,
    DecorationInputSpec, DecorationStateService, DecorationThemeExtension,
    DecorationVisualAdapterSpec, PaneActivity, PaneDecoration, PaneGeometry, PaneLifecycle,
    SetStyleError, ValidationError, ValidationResult,
};
use bmux_plugin::{AttachInputEvent, AttachInputResult, ServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostAsyncHandle, TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_scene_protocol::scene_protocol::{
    BorderGlyphs, Color, DecorationScene, GradientAxis, InputHook, InputHookEndpoint,
    InputHookFilter, InteractiveRegion, NamedColor, PaintCommand, Rect, Style, SurfaceDecoration,
    VisualAdapterRequest,
};
use uuid::Uuid;

#[cfg(test)]
use crate::engine::{AnimationDriverPolicy, run_animation_tick_if_current, with_engine_state};
use crate::engine::{
    DecorationEngineCommand, ensure_decoration_engine, send_engine_command,
    send_engine_command_blocking, send_engine_fire_and_forget,
};
use crate::scripting::{
    PerfTracker, ScriptBackend, ScriptComponentMessage, ScriptEventDelivery, ScriptEventMessage,
    ScriptHostAccess, ScriptMessage, ScriptRenderMessage, ScriptServiceCall, ScriptServiceGrant,
    bundled_decoration_scripts,
};

/// Reserved decoration component id that represents pane terminal content.
/// Components ordered below this anchor render before the PTY content;
/// components ordered above it render after the PTY content.
const PANE_CONTENT_COMPONENT_ID: &str = "pane.content";
const MAX_ANIMATION_HZ: u16 = 60;

/// Runtime state for one user-composable decoration component.
struct ScriptComponentRuntime {
    id: String,
    spec: DecorationComponentSpec,
    instance_id: String,
    backend: Option<Arc<dyn ScriptBackend>>,
    script_path: Option<PathBuf>,
    script_source_hash: Option<u64>,
    script_started_at: Option<Instant>,
    script_frame: u64,
    script_perf: Option<PerfTracker>,
    script_events: VecDeque<ScriptEventMessage>,
}

struct CompiledScriptInstance {
    backend: Option<Arc<dyn ScriptBackend>>,
    script_path: Option<PathBuf>,
    script_source_hash: Option<u64>,
    script_started_at: Option<Instant>,
}

impl std::fmt::Debug for ScriptComponentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptComponentRuntime")
            .field("id", &self.id)
            .field("spec", &self.spec)
            .field("instance_id", &self.instance_id)
            .field("script_path", &self.script_path)
            .field("script_source_hash", &self.script_source_hash)
            .field("script_frame", &self.script_frame)
            .finish_non_exhaustive()
    }
}

/// In-memory state store.
#[derive(Default)]
struct State {
    /// Per-pane overrides. Panes without an override fall through to
    /// [`State::default_border`].
    panes: HashMap<Uuid, PaneDecoration>,
    /// Per-surface live geometry observed from the attach runtime.
    ///
    /// Keys are attach surface ids, not pane ids. Tiled pane surfaces usually use
    /// the pane id as their surface id, but floating panes have distinct surface
    /// ids while still pointing at a pane-backed PTY.
    geometry: HashMap<Uuid, PaneGeometry>,
    /// Visible pane-backed surface ids in attach layout order. Target selectors
    /// use this for stable visible-index matching and script payload ordering.
    visible_surface_order: Vec<Uuid>,
    /// Per-pane focus/zoom/lifecycle. Kept separate from
    /// `panes` (style) so mutators don't have to allocate a
    /// `PaneDecoration` row just to flip a focus bit.
    activity: HashMap<Uuid, PaneActivity>,
    /// Global default, used for any pane without a specific override.
    /// `BorderStyle` has `@default ascii` in the BPDL schema, so
    /// `BorderStyle::default()` yields `BorderStyle::Ascii`.
    default_border: BorderStyle,
    /// Monotonic revision counter for published decoration scenes.
    /// Incremented only when generated scene output changes so consumers
    /// can discard stale snapshots cheaply without repainting no-op ticks.
    scene_revision: u64,
    /// Last generated scene output that was published on the retained bus.
    /// The revision field is ignored when comparing future scene output.
    last_published_scene: Option<DecorationScene>,
    /// Currently-loaded extension supplied through `theme-extension:apply`;
    /// `None` means "no extension observed; paint with built-in ASCII defaults".
    current_theme: Option<DecorationThemeExtension>,
    /// Compiled legacy decoration script, if any. `None` means the theme
    /// did not request a top-level script, or scripting was disabled at build
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
    /// Named user-composable decoration components exported by the active
    /// theme stack. These run after the static border and legacy script path,
    /// ordered by their relative `above` / `below` constraints.
    script_components: BTreeMap<String, ScriptComponentRuntime>,
    /// Monotonic generation used to stop stale subscription threads
    /// after a theme/script reload.
    script_subscription_generation: u64,
    /// Latest attach-local visual adapter metadata, keyed by request id.
    visual_projections: BTreeMap<String, serde_json::Value>,
    /// Latest attach-local visual adapter byte payloads, keyed by request id.
    visual_projection_bytes: BTreeMap<String, Arc<[u8]>>,
    /// Authoritative attach-local visual adapter metadata keyed by request id,
    /// then surface id. `visual_projections` keeps the latest entry per request
    /// as a compatibility/convenience view for focused-pane consumers.
    visual_projection_surfaces: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    /// Authoritative attach-local visual adapter byte payloads keyed by request
    /// id, then surface id.
    visual_projection_surface_bytes: BTreeMap<String, BTreeMap<String, Arc<[u8]>>>,
    /// Active animation tick rate. Threads exit when this value changes.
    animation_hz: Option<u16>,
    /// Monotonic token invalidating stale animation tick threads across
    /// theme reapplies, including reapplies that keep the same tick rate.
    animation_generation: u64,
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
            .field("visible_surface_order", &self.visible_surface_order)
            .field("activity", &self.activity)
            .field("default_border", &self.default_border)
            .field("scene_revision", &self.scene_revision)
            .field("current_theme", &self.current_theme)
            .field("script_path", &self.script_path)
            .field("script_source_hash", &self.script_source_hash)
            .field("script_frame", &self.script_frame)
            .field(
                "script_components",
                &self.script_components.keys().collect::<Vec<_>>(),
            )
            .field("animation_hz", &self.animation_hz)
            .field("animation_generation", &self.animation_generation)
            .finish_non_exhaustive()
    }
}

impl State {
    /// Borrow-or-create activity for `pane_id`. Caller must publish the scene
    /// when they observe a change.
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

#[derive(Clone, Debug)]
struct DecorationReadModel {
    panes: HashMap<Uuid, PaneDecoration>,
    geometry: HashMap<Uuid, PaneGeometry>,
    activity: HashMap<Uuid, PaneActivity>,
    default_border: BorderStyle,
    current_theme: Option<DecorationThemeExtension>,
    scene: DecorationScene,
}

impl Default for DecorationReadModel {
    fn default() -> Self {
        Self {
            panes: HashMap::new(),
            geometry: HashMap::new(),
            activity: HashMap::new(),
            default_border: BorderStyle::default(),
            current_theme: None,
            scene: empty_scene(),
        }
    }
}

struct DecorationRuntime {
    command_tx: Mutex<Option<std::sync::mpsc::SyncSender<DecorationEngineCommand>>>,
    read_model: ArcSwap<DecorationReadModel>,
    host_async_handle: Mutex<Option<HostAsyncHandle>>,
}

impl Default for DecorationRuntime {
    fn default() -> Self {
        Self {
            command_tx: Mutex::new(None),
            read_model: ArcSwap::from_pointee(DecorationReadModel::default()),
            host_async_handle: Mutex::new(None),
        }
    }
}

/// Shared decoration runtime handle.
///
/// Production callers never own or lock [`State`] directly. They either send
/// commands to the decoration engine or read the latest projected
/// [`DecorationReadModel`].
#[derive(Clone, Default)]
struct SharedState {
    runtime: Arc<DecorationRuntime>,
}

impl SharedState {
    fn new() -> Self {
        Self::default()
    }

    fn command_tx(&self) -> Option<std::sync::mpsc::SyncSender<DecorationEngineCommand>> {
        self.runtime
            .command_tx
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn set_command_tx(&self, tx: std::sync::mpsc::SyncSender<DecorationEngineCommand>) -> bool {
        let Ok(mut guard) = self.runtime.command_tx.lock() else {
            return false;
        };
        if guard.is_some() {
            return false;
        }
        *guard = Some(tx);
        true
    }

    fn set_host_async_handle(&self, handle: HostAsyncHandle) {
        if let Ok(mut guard) = self.runtime.host_async_handle.lock() {
            *guard = Some(handle);
        }
    }

    fn host_async_handle(&self) -> Option<HostAsyncHandle> {
        self.runtime
            .host_async_handle
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn read_model<R>(&self, f: impl FnOnce(&DecorationReadModel) -> R) -> R {
        let model = self.runtime.read_model.load();
        f(&model)
    }

    fn sync_read_model_from_state(&self, state: &mut State) {
        self.runtime
            .read_model
            .store(Arc::new(read_model_from_state(state)));
    }

    #[cfg(test)]
    fn with_state<R: Send + 'static>(&self, f: impl FnOnce(&mut State) -> R + Send + 'static) -> R {
        with_engine_state(self, f)
    }
}

fn read_model_from_state(state: &mut State) -> DecorationReadModel {
    DecorationReadModel {
        panes: state.panes.clone(),
        geometry: state.geometry.clone(),
        activity: state.activity.clone(),
        default_border: state.default_border,
        current_theme: state.current_theme.clone(),
        scene: scene_snapshot_from_state(state),
    }
}

fn pane_decoration_from_read_model(model: &DecorationReadModel, pane_id: Uuid) -> PaneDecoration {
    model.panes.get(&pane_id).cloned().unwrap_or_else(|| {
        let focused = model.activity.get(&pane_id).is_some_and(|a| a.focused);
        default_pane_decoration(pane_id, model.default_border, focused)
    })
}

fn geometry_for_pane_read_model(
    model: &DecorationReadModel,
    pane_id: Uuid,
) -> Option<PaneGeometry> {
    model
        .geometry
        .values()
        .find(|geometry| geometry.pane_id == pane_id)
        .cloned()
}

/// Typed-service provider handle.
///
/// Wraps the shared decoration runtime so typed service consumers read from
/// the latest read model and send mutations through the engine command loop.
struct DecorationServiceHandle {
    state: SharedState,
}

impl DecorationServiceHandle {
    fn new(state: SharedState) -> Self {
        Self { state }
    }
}

impl DecorationStateService for DecorationServiceHandle {
    fn pane_decoration<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneDecoration>> + Send + 'a>> {
        Box::pin(async move {
            Some(
                self.state
                    .read_model(|model| pane_decoration_from_read_model(model, pane_id)),
            )
        })
    }

    fn default_border_style<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = BorderStyle> + Send + 'a>> {
        Box::pin(async move { self.state.read_model(|model| model.default_border) })
    }

    fn scene_snapshot<'a>(&'a self) -> Pin<Box<dyn Future<Output = DecorationScene> + Send + 'a>> {
        Box::pin(async move { self.state.read_model(|model| model.scene.clone()) })
    }

    fn pane_geometry<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneGeometry>> + Send + 'a>> {
        Box::pin(async move {
            self.state
                .read_model(|model| geometry_for_pane_read_model(model, pane_id))
        })
    }

    fn pane_activity<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneActivity>> + Send + 'a>> {
        Box::pin(async move {
            self.state
                .read_model(|model| model.activity.get(&pane_id).cloned())
        })
    }

    fn current_theme_extension<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<DecorationThemeExtension>> + Send + 'a>> {
        Box::pin(async move { self.state.read_model(|model| model.current_theme.clone()) })
    }

    fn validate_theme_extension<'a>(
        &'a self,
        toml_text: String,
    ) -> Pin<Box<dyn Future<Output = ValidationResult> + Send + 'a>> {
        Box::pin(async move { validate_theme_extension_toml(&toml_text) })
    }
}

impl DecorationCommandsService for DecorationServiceHandle {
    fn set_pane_border<'a>(
        &'a self,
        pane_id: Uuid,
        border: BorderStyle,
    ) -> Pin<Box<dyn Future<Output = Result<(), SetStyleError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(outcome) = send_engine_command(&self.state, |reply| {
                DecorationEngineCommand::SetPaneBorder {
                    pane_id,
                    border,
                    reply,
                }
            })
            .await
            {
                return outcome;
            }
            Err(SetStyleError::StyleUnsupported {
                style: "decoration engine unavailable".to_string(),
            })
        })
    }

    fn set_default_border<'a>(
        &'a self,
        border: BorderStyle,
    ) -> Pin<Box<dyn Future<Output = Result<(), SetStyleError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(outcome) = send_engine_command(&self.state, |reply| {
                DecorationEngineCommand::SetDefaultBorder { border, reply }
            })
            .await
            {
                return outcome;
            }
            Err(SetStyleError::StyleUnsupported {
                style: "decoration engine unavailable".to_string(),
            })
        })
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
            if let Some(outcome) = send_engine_command(&self.state, |reply| {
                DecorationEngineCommand::ApplyThemeExtension {
                    toml_text: toml_text.clone(),
                    config_dir_candidates: candidates.clone(),
                    script_host_access: ScriptHostAccess::default(),
                    reply,
                }
            })
            .await
            {
                return outcome;
            }
            Err(ValidationResult::Errors {
                errors: vec![ValidationError {
                    path: "<engine>".to_string(),
                    message: "decoration engine unavailable".to_string(),
                }],
            })
        })
    }

    fn notify_pane_event<'a>(
        &'a self,
        event: PaneEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(outcome) = send_engine_command(&self.state, |reply| {
                DecorationEngineCommand::NotifyPaneEvent {
                    event: event.clone(),
                    reply,
                }
            })
            .await
            {
                return outcome;
            }
            Err(NotifyError::InvalidArgument {
                reason: "decoration engine unavailable".to_string(),
            })
        })
    }
}

// Direct mutation helper for engine dispatch, pre-engine fallback paths, and
// tests. Normal post-activation runtime mutations should enter through
// `DecorationEngineCommand`.
fn set_pane_border_direct(state: &mut State, pane_id: Uuid, border: BorderStyle) -> bool {
    let focused = state.activity.get(&pane_id).is_some_and(|a| a.focused);
    let had_entry = state.panes.contains_key(&pane_id);
    let entry = state
        .panes
        .entry(pane_id)
        .or_insert_with(|| default_pane_decoration(pane_id, border, focused));
    if had_entry && entry.border == border && entry.focused == focused {
        return false;
    }
    entry.border = border;
    entry.focused = focused;
    publish_scene_if_changed(state);
    true
}

// Direct mutation helper for engine dispatch, pre-engine fallback paths, and
// tests. Normal post-activation runtime mutations should enter through
// `DecorationEngineCommand`.
fn set_default_border_direct(state: &mut State, border: BorderStyle) -> bool {
    if state.default_border == border {
        return false;
    }
    state.default_border = border;
    publish_scene_if_changed(state);
    true
}

// Direct mutation helper for engine dispatch, pre-engine fallback paths, and
// tests. Normal post-activation runtime mutations should enter through
// `DecorationEngineCommand`.
fn notify_pane_event_direct(state: &mut State, event: &PaneEvent) -> bool {
    apply_pane_event(state, event)
}

fn handle_attach_input_event(state: &mut State, event: AttachInputEvent) -> AttachInputResult {
    let mut result = AttachInputResult::default();
    let message = ScriptMessage::Input(event);
    let mut invoked_instances = BTreeSet::<String>::new();
    if state
        .current_theme
        .as_ref()
        .and_then(|theme| theme.input.as_ref())
        .is_some()
        && let Some(backend) = state.script_backend.as_ref()
    {
        match backend.invoke(&message) {
            Ok(outcome) => {
                merge_attach_input_result(&mut result, outcome.input_result);
                record_script_perf(state, outcome.duration);
            }
            Err(error) => {
                tracing::warn!(target: "decoration.script", error = %error, "decoration script input invocation failed");
            }
        }
    }

    for component in state.script_components.values_mut() {
        if component.spec.input.is_none() {
            continue;
        }
        let Some(backend) = component.backend.as_ref() else {
            continue;
        };
        if !invoked_instances.insert(component.instance_id.clone()) {
            continue;
        }
        match backend.invoke(&message) {
            Ok(outcome) => {
                merge_attach_input_result(&mut result, outcome.input_result);
                record_component_script_perf(component, outcome.duration);
            }
            Err(error) => {
                tracing::warn!(
                    target: "decoration.script",
                    component_id = %component.id,
                    error = %error,
                    "decoration component input invocation failed",
                );
            }
        }
    }

    if result.dirty {
        publish_scene_if_changed(state);
    }
    result
}

fn merge_attach_input_result(result: &mut AttachInputResult, next: Option<AttachInputResult>) {
    let Some(next) = next else {
        return;
    };
    result.consumed |= next.consumed;
    result.capture_pointer |= next.capture_pointer;
    result.release_capture |= next.release_capture;
    result.dirty |= next.dirty;
    for key in next.capture_keyboard {
        if !result
            .capture_keyboard
            .iter()
            .any(|existing| existing == &key)
        {
            result.capture_keyboard.push(key);
        }
    }
}

/// Apply a [`PaneEvent`] to the shared state. Pulled out so both the
/// typed `notify_pane_event` command and the event-bus subscriber
/// can share the same mutation path.
fn apply_pane_event(state: &mut State, event: &PaneEvent) -> bool {
    let mut changed = false;
    match event {
        PaneEvent::Focused { pane_id } => {
            // Unfocus every other pane so the activity map has a
            // single focused entry at most.
            for (id, act) in &mut state.activity {
                if *id != *pane_id && act.focused {
                    act.focused = false;
                    changed = true;
                }
            }
            let act = state.activity_mut(*pane_id);
            if !act.focused {
                act.focused = true;
                changed = true;
            }
            state.sync_focused_mirror(*pane_id, true);
        }
        PaneEvent::Unfocused { pane_id } => {
            if let Some(act) = state.activity.get_mut(pane_id)
                && act.focused
            {
                act.focused = false;
                changed = true;
            }
            state.sync_focused_mirror(*pane_id, false);
        }
        PaneEvent::Zoomed { pane_id } => {
            let act = state.activity_mut(*pane_id);
            if !act.zoomed {
                act.zoomed = true;
                changed = true;
            }
        }
        PaneEvent::Unzoomed { pane_id } => {
            if let Some(act) = state.activity.get_mut(pane_id)
                && act.zoomed
            {
                act.zoomed = false;
                changed = true;
            }
        }
        PaneEvent::Opened { pane_id, .. } => {
            changed = !state.activity.contains_key(pane_id);
            state.activity_mut(*pane_id);
        }
        PaneEvent::Closed { pane_id } => {
            let panes_removed = state.panes.remove(pane_id).is_some();
            let geometry_len = state.geometry.len();
            state.geometry.retain(|_, geom| geom.pane_id != *pane_id);
            state
                .visible_surface_order
                .retain(|surface_id| state.geometry.contains_key(surface_id));
            let geometry_removed = state.geometry.len() != geometry_len;
            let activity_removed = state.activity.remove(pane_id).is_some();
            changed = panes_removed || geometry_removed || activity_removed;
        }
        PaneEvent::StatusChanged { pane_id, exited } => {
            let status = if *exited {
                PaneLifecycle::Exited
            } else {
                PaneLifecycle::Running
            };
            let act = state.activity_mut(*pane_id);
            if act.status != status {
                act.status = status;
                changed = true;
            }
        }
    }
    if changed {
        publish_scene_if_changed(state);
    }
    changed
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
    let surface_ids: Vec<Uuid> = state.geometry.keys().copied().collect();
    for surface_id in surface_ids {
        let Some(geom) = state.geometry.get(&surface_id).cloned() else {
            continue;
        };
        let pane_id = geom.pane_id;
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
            surface_id,
            SurfaceDecoration {
                surface_id,
                rect,
                content_rect,
                paint_commands,
                before_content_paint_commands: Vec::new(),
                interactive_regions,
            },
        );
    }
    merge_script_paint_commands(state, &mut surfaces);
    merge_component_paint_commands(state, &mut surfaces);
    DecorationScene {
        revision: state.scene_revision,
        surfaces,
        animation: None,
        input_hooks: input_hooks_for_state(state),
        visual_adapters: visual_adapter_requests_for_state(state),
    }
}

fn geometry_for_pane(state: &State, pane_id: Uuid) -> Option<&PaneGeometry> {
    state
        .geometry
        .values()
        .find(|geometry| geometry.pane_id == pane_id)
}

fn script_pane_payload_for_surface(
    state: &State,
    surface_id: Uuid,
    visible_index: usize,
    component_active: bool,
) -> Option<serde_json::Value> {
    let geom = state.geometry.get(&surface_id)?;
    let pane_id = geom.pane_id;
    let activity = state.activity.get(&pane_id);
    let (focused, zoomed) = activity.map_or((false, false), |a| (a.focused, a.zoomed));
    let status = activity.map_or(PaneLifecycle::Running, |a| a.status);
    let mut payload = serde_json::json!({
        "id": surface_id.to_string(),
        "pane_id": pane_id.to_string(),
        "surface_id": surface_id.to_string(),
        "visible_index": visible_index,
        "rect": rect_json(&geom.rect),
        "content_rect": rect_json(&geom.content_rect),
        "focused": focused,
        "zoomed": zoomed,
        "status": match status {
            PaneLifecycle::Running => "running",
            PaneLifecycle::Exited => "exited",
        },
    });
    if component_active && let serde_json::Value::Object(object) = &mut payload {
        object.insert(
            "component_active".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    Some(payload)
}

fn component_target_matches(
    state: &State,
    target: Option<&DecorationComponentTargetSpec>,
    surface_id: Uuid,
    visible_index: usize,
) -> bool {
    let Some(target) = target else {
        return true;
    };
    let kind = target.kind.trim();
    if kind.eq_ignore_ascii_case("all-panes") || kind.eq_ignore_ascii_case("all") {
        return true;
    }
    let focused = state
        .geometry
        .get(&surface_id)
        .and_then(|geometry| state.activity.get(&geometry.pane_id))
        .is_some_and(|activity| activity.focused);
    if kind.eq_ignore_ascii_case("focused-pane") || kind.eq_ignore_ascii_case("focused") {
        return focused;
    }
    if kind.eq_ignore_ascii_case("unfocused-panes") || kind.eq_ignore_ascii_case("unfocused") {
        return !focused;
    }
    if kind.eq_ignore_ascii_case("visible-index") {
        return target
            .index
            .is_some_and(|index| usize::from(index) == visible_index);
    }
    if kind.eq_ignore_ascii_case("visible-index-mod") {
        let modulo = target.modulo.filter(|value| *value > 0).unwrap_or(1);
        let remainder = target.remainder.unwrap_or(0) % modulo;
        let Ok(index) = u16::try_from(visible_index) else {
            return false;
        };
        return index % modulo == remainder;
    }
    false
}

fn script_visual_payload(state: &State) -> serde_json::Value {
    serde_json::Value::Object(
        state
            .visual_projections
            .iter()
            .map(|(key, value)| {
                let mut value = value.clone();
                if let Some(surfaces) = state.visual_projection_surfaces.get(key)
                    && let serde_json::Value::Object(object) = &mut value
                {
                    object.insert(
                        "surfaces".to_string(),
                        serde_json::Value::Object(
                            surfaces
                                .iter()
                                .map(|(surface_id, metadata)| {
                                    (surface_id.clone(), metadata.clone())
                                })
                                .collect(),
                        ),
                    );
                }
                (key.clone(), value)
            })
            .collect(),
    )
}

fn script_visual_bytes_payload(state: &State) -> BTreeMap<String, Arc<[u8]>> {
    state.visual_projection_bytes.clone()
}

fn script_visual_surface_bytes_payload(
    state: &State,
) -> BTreeMap<String, BTreeMap<String, Arc<[u8]>>> {
    state.visual_projection_surface_bytes.clone()
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
        visual: script_visual_payload(state),
        visual_bytes: script_visual_bytes_payload(state),
        visual_surface_bytes: script_visual_surface_bytes_payload(state),
        component: None,
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

struct ComponentRenderPassState {
    visual: serde_json::Value,
    visual_bytes: BTreeMap<String, Arc<[u8]>>,
    visual_surface_bytes: BTreeMap<String, BTreeMap<String, Arc<[u8]>>>,
    instance_frames: BTreeMap<String, (u64, u64)>,
    event_instances: BTreeSet<String>,
}

fn merge_component_paint_commands(
    state: &mut State,
    surfaces: &mut BTreeMap<Uuid, SurfaceDecoration>,
) {
    let order = ordered_enabled_component_ids(state);
    let visual = script_visual_payload(state);
    let visual_bytes = script_visual_bytes_payload(state);
    let visual_surface_bytes = script_visual_surface_bytes_payload(state);
    let mut pass = ComponentRenderPassState {
        visual,
        visual_bytes,
        visual_surface_bytes,
        instance_frames: BTreeMap::new(),
        event_instances: BTreeSet::new(),
    };
    for (index, component_id) in order.iter().enumerate() {
        if component_id == PANE_CONTENT_COMPONENT_ID {
            continue;
        }
        let below_pane_content = order
            .iter()
            .position(|id| id == component_id)
            .zip(order.iter().position(|id| id == PANE_CONTENT_COMPONENT_ID))
            .is_some_and(|(component_index, content_index)| component_index < content_index);
        merge_component_paint_commands_for_id(
            state,
            surfaces,
            component_id,
            index,
            below_pane_content,
            &mut pass,
        );
    }
}

fn merge_component_paint_commands_for_id(
    state: &mut State,
    surfaces: &mut BTreeMap<Uuid, SurfaceDecoration>,
    component_id: &str,
    order_index: usize,
    below_pane_content: bool,
    pass: &mut ComponentRenderPassState,
) {
    let target = state
        .script_components
        .get(component_id)
        .and_then(|component| component.spec.target.clone());
    let panes = script_panes_payload_for_component(state, target.as_ref());
    let Some(component) = state.script_components.get_mut(component_id) else {
        return;
    };
    let Some(backend) = component.backend.as_ref() else {
        return;
    };
    if pass.event_instances.insert(component.instance_id.clone()) {
        while let Some(event) = component.script_events.pop_front() {
            let message = ScriptMessage::Event(event);
            let outcome = match backend.invoke(&message) {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!(
                        target: "decoration.script",
                        component_id,
                        error = %error,
                        "decoration component event invocation failed",
                    );
                    continue;
                }
            };
            record_component_script_perf(component, outcome.duration);
        }
    } else {
        component.script_events.clear();
    }

    let (frame, time_ms) =
        if let Some((frame, time_ms)) = pass.instance_frames.get(&component.instance_id).copied() {
            (frame, time_ms)
        } else {
            component.script_frame = component.script_frame.saturating_add(1);
            let time_ms = component.script_started_at.map_or(0, |started_at| {
                u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            let frame = component.script_frame;
            pass.instance_frames
                .insert(component.instance_id.clone(), (frame, time_ms));
            (frame, time_ms)
        };
    let render = ScriptMessage::Render(ScriptRenderMessage {
        time_ms,
        frame,
        panes,
        visual: pass.visual.clone(),
        visual_bytes: pass.visual_bytes.clone(),
        visual_surface_bytes: pass.visual_surface_bytes.clone(),
        component: Some(ScriptComponentMessage {
            id: component.id.clone(),
            entrypoint: component.spec.entrypoint.clone(),
            settings: component_settings_json(&component.spec),
        }),
    });
    let outcome = match backend.invoke(&render) {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                target: "decoration.script",
                component_id,
                error = %error,
                "decoration component render invocation failed",
            );
            return;
        }
    };
    record_component_script_perf(component, outcome.duration);
    for (pane_id, mut commands) in outcome.surfaces {
        let Ok(pane_id) = pane_id.parse::<Uuid>() else {
            tracing::warn!(target: "decoration.script", pane_id, component_id, "component returned unknown pane id");
            continue;
        };
        normalize_component_command_z(&mut commands, order_index);
        let surface = surfaces
            .entry(pane_id)
            .or_insert_with(|| empty_surface_for(state, pane_id));
        if below_pane_content {
            surface.before_content_paint_commands.extend(commands);
        } else {
            surface.paint_commands.extend(commands);
        }
    }
}

fn component_settings_json(spec: &DecorationComponentSpec) -> serde_json::Value {
    serde_json::Value::Object(
        spec.settings
            .as_ref()
            .map(|settings| {
                settings
                    .iter()
                    .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
                    .collect()
            })
            .unwrap_or_default(),
    )
}

fn record_component_script_perf(component: &ScriptComponentRuntime, duration: Duration) {
    if let Some(tracker) = component.script_perf.as_ref()
        && let Some(msg) = tracker.record(duration)
    {
        tracing::warn!(target: "decoration.script", component_id = %component.id, "{msg}");
    }
}

fn ordered_enabled_component_ids(state: &State) -> Vec<String> {
    let mut enabled = state
        .script_components
        .iter()
        .filter(|(_, component)| component_enabled(&component.spec))
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    enabled.insert(PANE_CONTENT_COMPONENT_ID.to_string());
    let mut incoming = enabled
        .iter()
        .map(|id| (id.clone(), BTreeSet::<String>::new()))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing = enabled
        .iter()
        .map(|id| (id.clone(), BTreeSet::<String>::new()))
        .collect::<BTreeMap<_, _>>();

    for id in &enabled {
        let Some(component) = state.script_components.get(id) else {
            continue;
        };
        for target in component.spec.above.as_deref().unwrap_or_default() {
            if enabled.contains(target) {
                add_component_order_edge(target, id, &mut incoming, &mut outgoing);
            }
        }
        for target in component.spec.below.as_deref().unwrap_or_default() {
            if enabled.contains(target) {
                add_component_order_edge(id, target, &mut incoming, &mut outgoing);
            }
        }
    }

    let mut ready = incoming
        .iter()
        .filter(|(_, deps)| deps.is_empty())
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    let mut ordered = Vec::with_capacity(enabled.len());
    while let Some(id) = pop_next_component_id(&mut ready) {
        ordered.push(id.clone());
        let dependents = outgoing.remove(&id).unwrap_or_default();
        for dependent in dependents {
            let Some(deps) = incoming.get_mut(&dependent) else {
                continue;
            };
            deps.remove(&id);
            if deps.is_empty() {
                ready.insert(dependent);
            }
        }
    }
    if ordered.len() != enabled.len() {
        let unresolved = enabled
            .into_iter()
            .filter(|id| !ordered.iter().any(|ordered_id| ordered_id == id))
            .collect::<Vec<_>>();
        tracing::warn!(
            target: "decoration.script",
            unresolved = ?unresolved,
            "component layering cycle detected; appending unresolved components in id order",
        );
        ordered.extend(unresolved);
    }
    ordered
}

fn pop_next_component_id(ready: &mut BTreeSet<String>) -> Option<String> {
    if ready.remove(PANE_CONTENT_COMPONENT_ID) {
        return Some(PANE_CONTENT_COMPONENT_ID.to_string());
    }
    ready.pop_first()
}

fn add_component_order_edge(
    before: &str,
    after: &str,
    incoming: &mut BTreeMap<String, BTreeSet<String>>,
    outgoing: &mut BTreeMap<String, BTreeSet<String>>,
) {
    if before == after {
        return;
    }
    if let Some(deps) = incoming.get_mut(after) {
        deps.insert(before.to_string());
    }
    if let Some(dependents) = outgoing.get_mut(before) {
        dependents.insert(after.to_string());
    }
}

fn component_enabled(spec: &DecorationComponentSpec) -> bool {
    spec.enabled.unwrap_or(true)
}

fn normalize_component_command_z(commands: &mut [PaintCommand], order_index: usize) {
    if commands.is_empty() {
        return;
    }
    let min_z = commands.iter().map(paint_command_z).min().unwrap_or(0);
    let base = component_base_z(order_index);
    for command in commands {
        let relative = paint_command_z(command).saturating_sub(min_z).clamp(0, 99);
        set_paint_command_z(command, base.saturating_add(relative));
    }
}

fn component_base_z(order_index: usize) -> i16 {
    let index = i16::try_from(order_index).unwrap_or(i16::MAX / 100);
    100_i16.saturating_add(index.saturating_mul(100))
}

const fn paint_command_z(command: &PaintCommand) -> i16 {
    match command {
        PaintCommand::Text { z, .. }
        | PaintCommand::FilledRect { z, .. }
        | PaintCommand::GradientRun { z, .. }
        | PaintCommand::CellGrid { z, .. }
        | PaintCommand::BoxBorder { z, .. } => *z,
    }
}

fn set_paint_command_z(command: &mut PaintCommand, next_z: i16) {
    match command {
        PaintCommand::Text { z, .. }
        | PaintCommand::FilledRect { z, .. }
        | PaintCommand::GradientRun { z, .. }
        | PaintCommand::CellGrid { z, .. }
        | PaintCommand::BoxBorder { z, .. } => *z = next_z,
    }
}

fn script_panes_payload(state: &State) -> serde_json::Value {
    script_panes_payload_for_component(state, None)
}

fn script_panes_payload_for_component(
    state: &State,
    target: Option<&DecorationComponentTargetSpec>,
) -> serde_json::Value {
    serde_json::Value::Array(
        ordered_surface_ids(state)
            .into_iter()
            .enumerate()
            .filter_map(|(visible_index, surface_id)| {
                let matches = component_target_matches(state, target, surface_id, visible_index);
                if target.is_some() && !matches {
                    return None;
                }
                script_pane_payload_for_surface(state, surface_id, visible_index, target.is_some())
            })
            .collect(),
    )
}

fn ordered_surface_ids(state: &State) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(state.geometry.len());
    let mut seen = BTreeSet::new();
    for surface_id in &state.visible_surface_order {
        if state.geometry.contains_key(surface_id) && seen.insert(*surface_id) {
            ids.push(*surface_id);
        }
    }
    let mut remaining = state
        .geometry
        .keys()
        .filter(|surface_id| !seen.contains(surface_id))
        .copied()
        .collect::<Vec<_>>();
    remaining.sort_unstable();
    ids.extend(remaining);
    ids
}

fn record_script_perf(state: &State, duration: Duration) {
    if let Some(tracker) = state.script_perf.as_ref()
        && let Some(msg) = tracker.record(duration)
    {
        tracing::warn!(target: "decoration.script", "{msg}");
    }
}

fn input_hooks_for_state(state: &State) -> Vec<InputHook> {
    let Some(theme) = state.current_theme.as_ref() else {
        return Vec::new();
    };
    let mut specs = Vec::new();
    if let Some(input) = theme.input.as_ref() {
        specs.push(input);
    }
    if let Some(components) = theme.components.as_ref() {
        specs.extend(
            components
                .values()
                .filter_map(|component| component.input.as_ref()),
        );
    }
    let Some(filter) = combine_input_specs(&specs) else {
        return Vec::new();
    };
    vec![InputHook {
        id: "bmux.decoration.input".to_string(),
        owner_plugin_id: DECORATION_PLUGIN_ID.to_string(),
        priority: specs
            .iter()
            .filter_map(|spec| spec.priority)
            .max()
            .unwrap_or(0),
        endpoint: InputHookEndpoint {
            capability: "bmux.decoration.write".to_string(),
            interface_id: DECORATION_INPUT_INTERFACE_ID.to_string(),
            operation: DECORATION_INPUT_OPERATION.to_string(),
        },
        filter,
    }]
}

fn visual_adapter_requests_for_state(state: &State) -> Vec<VisualAdapterRequest> {
    let Some(theme) = state.current_theme.as_ref() else {
        return Vec::new();
    };
    let mut requests = Vec::new();
    if let Some(components) = theme.components.as_ref() {
        for component in components.values() {
            let Some(specs) = component.visual_adapters.as_ref() else {
                continue;
            };
            for spec in specs {
                requests.push(visual_adapter_request_from_spec(spec));
            }
        }
    }
    requests
}

fn visual_adapter_request_from_spec(spec: &DecorationVisualAdapterSpec) -> VisualAdapterRequest {
    VisualAdapterRequest {
        id: spec.id.clone(),
        adapter: spec.adapter.clone(),
        owner_plugin_id: DECORATION_PLUGIN_ID.to_string(),
        event_kind: "bmux.decoration/visual-projection".to_string(),
        scope: spec
            .scope
            .clone()
            .unwrap_or_else(|| "focused-pane".to_string()),
        area: spec.area.clone().unwrap_or_else(|| "content".to_string()),
        max_hz: spec.max_hz.unwrap_or(30),
        dirty_only: spec.dirty_only.unwrap_or(true),
        max_bytes: spec.max_bytes.unwrap_or(16 * 1024),
        settings: spec.settings.clone().unwrap_or_default(),
    }
}

fn combine_input_specs(specs: &[&DecorationInputSpec]) -> Option<InputHookFilter> {
    let mut mouse = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut scope = "focused-pane".to_string();
    let mut min_interval_ms = u16::MAX;
    for spec in specs {
        mouse.extend(spec.mouse.as_deref().unwrap_or_default().iter().cloned());
        keys.extend(spec.keys.as_deref().unwrap_or_default().iter().cloned());
        if let Some(spec_scope) = spec.scope.as_deref()
            && (spec_scope == "global" || (spec_scope == "hovered-pane" && scope != "global"))
        {
            scope = spec_scope.to_string();
        }
        if let Some(interval) = spec.min_interval_ms
            && interval > 0
        {
            min_interval_ms = min_interval_ms.min(interval);
        }
    }
    if mouse.is_empty() && keys.is_empty() {
        return None;
    }
    if min_interval_ms == u16::MAX {
        min_interval_ms = 0;
    }
    Some(InputHookFilter {
        mouse_phases: mouse.into_iter().collect(),
        keys: keys.into_iter().collect(),
        scope,
        min_interval_ms,
    })
}

fn empty_surface_for(state: &State, pane_id: Uuid) -> SurfaceDecoration {
    let (rect, content_rect) = geometry_for_pane(state, pane_id).map_or_else(
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
        before_content_paint_commands: Vec::new(),
        interactive_regions: Vec::new(),
    }
}

/// Identifier the decoration plugin attributes to its own generic hooks.
const DECORATION_PLUGIN_ID: &str = "bmux.decoration";
const DECORATION_INPUT_INTERFACE_ID: &str = "decoration-input-hooks";
const DECORATION_INPUT_OPERATION: &str = "handle-input";
const DECORATION_VISUAL_PROJECTION_KIND: &str = "bmux.decoration/visual-projection";

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
        input_hooks: Vec::new(),
        visual_adapters: Vec::new(),
    }
}

/// Publish the updated [`DecorationScene`] as retained state on the typed plugin
/// event bus when generated output changed. Called from every mutator so
/// consumers (e.g. the attach runtime) can refresh their scene cache
/// incrementally while late subscribers can still hydrate from the current
/// value. Publication silently no-ops if the event-bus channel has not been
/// registered yet (the decoration plugin registers it in
/// [`DecorationPlugin::activate`]).
fn publish_scene_if_changed(state: &mut State) -> bool {
    // Build while we still hold the lock so script render output and revision
    // updates stay ordered from subscribers' perspective. The candidate scene
    // is compared without its revision so animation ticks that do not change
    // visible output do not force attach clients to repaint.
    let mut scene = build_scene(state);
    if state
        .last_published_scene
        .as_ref()
        .is_some_and(|previous| scene_output_matches(previous, &scene))
    {
        return false;
    }

    state.scene_revision = state.scene_revision.saturating_add(1);
    scene.revision = state.scene_revision;
    state.last_published_scene = Some(scene.clone());
    let _ = bmux_plugin::global_event_bus()
        .publish_state(&bmux_scene_protocol::scene_protocol::STATE_KIND, scene);
    true
}

fn scene_output_matches(left: &DecorationScene, right: &DecorationScene) -> bool {
    left.surfaces == right.surfaces
        && left.animation == right.animation
        && left.input_hooks == right.input_hooks
        && left.visual_adapters == right.visual_adapters
}

fn scene_snapshot_from_state(state: &mut State) -> DecorationScene {
    state
        .last_published_scene
        .clone()
        .unwrap_or_else(|| build_scene(state))
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
    state: &SharedState,
    text: &str,
    config_dir_candidates: &[PathBuf],
    script_host_access: &ScriptHostAccess,
) -> Result<(), ValidationResult> {
    send_engine_command_blocking(state, |reply| {
        DecorationEngineCommand::ApplyThemeExtension {
            toml_text: text.to_string(),
            config_dir_candidates: config_dir_candidates.to_vec(),
            script_host_access: script_host_access.clone(),
            reply,
        }
    })
    .unwrap_or_else(|| {
        Err(ValidationResult::Errors {
            errors: vec![ValidationError {
                path: "<engine>".to_string(),
                message: "decoration engine unavailable".to_string(),
            }],
        })
    })
}

fn apply_theme_extension_toml_direct(
    shared: &SharedState,
    state: &mut State,
    text: &str,
    config_dir_candidates: &[PathBuf],
    script_host_access: &ScriptHostAccess,
) -> Result<(), ValidationResult> {
    if text.trim().is_empty() {
        state.current_theme = None;
        state.animation_hz = None;
        state.animation_generation = state.animation_generation.saturating_add(1);
        state.script_components.clear();
        clear_visual_projection_state(state);
        install_script_backend(state, None, ScriptHostAccess::default());
        publish_scene_if_changed(state);
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
    let animation_hz = effective_animation_hz(&extension);
    let script_access = extension.script_access.clone();
    let mut script_host_access = script_host_access.clone();
    script_host_access.service_grants = script_service_grants(script_access.as_ref());
    install_script_components(
        state,
        &extension,
        config_dir_candidates,
        &script_host_access,
    );
    state.current_theme = Some(extension);
    clear_visual_projection_state(state);
    state.animation_hz = animation_hz;
    state.animation_generation = state.animation_generation.saturating_add(1);
    install_script_backend(state, script, script_host_access.clone());
    let subscription_generation = state.script_subscription_generation;
    publish_scene_if_changed(state);
    install_script_event_subscriptions(shared, state, script_access, subscription_generation);
    Ok(())
}

fn effective_animation_hz(extension: &DecorationThemeExtension) -> Option<u16> {
    let top_level = extension.animation.as_ref().map(|animation| animation.hz);
    let component_level = extension.components.as_ref().and_then(|components| {
        components
            .values()
            .filter(|component| component_enabled(component))
            .filter_map(|component| component.animation.as_ref().map(|animation| animation.hz))
            .max()
    });
    top_level
        .into_iter()
        .chain(component_level)
        .filter(|hz| *hz > 0)
        .max()
        .map(|hz| hz.min(MAX_ANIMATION_HZ))
}

fn clear_visual_projection_state(state: &mut State) {
    state.visual_projections.clear();
    state.visual_projection_bytes.clear();
    state.visual_projection_surfaces.clear();
    state.visual_projection_surface_bytes.clear();
}

fn script_service_grants(
    access: Option<&bmux_decoration_plugin_api::decoration_state::ScriptAccessSpec>,
) -> Vec<ScriptServiceGrant> {
    access.map_or_else(Vec::new, |access| {
        access
            .services
            .iter()
            .map(|grant| ScriptServiceGrant {
                capability: grant.capability.clone(),
                kind: grant.kind.clone(),
                interface: grant.interface_id.clone(),
                operation: grant.operation.clone(),
            })
            .collect()
    })
}

fn script_host_access_from_context(context: &NativeServiceContext) -> ScriptHostAccess {
    let context = context.clone();
    ScriptHostAccess {
        service_grants: Vec::new(),
        service_caller: Some(std::sync::Arc::new(move |call: ScriptServiceCall| {
            let kind = parse_script_service_kind(&call.kind)?;
            let payload = bmux_plugin_sdk::encode_service_message(&call.payload)
                .map_err(|error| error.to_string())?;
            let response = context
                .call_service_raw(
                    &call.capability,
                    kind,
                    &call.interface,
                    &call.operation,
                    payload,
                )
                .map_err(|error| error.to_string())?;
            bmux_plugin_sdk::decode_service_message::<serde_json::Value>(&response)
                .map_err(|error| error.to_string())
        })),
    }
}

fn parse_script_service_kind(kind: &str) -> Result<ServiceKind, String> {
    match kind {
        "query" => Ok(ServiceKind::Query),
        "command" => Ok(ServiceKind::Command),
        "event" => Ok(ServiceKind::Event),
        other => Err(format!("unsupported service kind {other:?}")),
    }
}

const MAX_SCRIPT_EVENT_QUEUE: usize = 256;
const DEFAULT_STARTUP_READY_GATE_TIMEOUT: Duration = Duration::from_secs(2);

fn install_script_event_subscriptions(
    shared: &SharedState,
    state: &mut State,
    access: Option<bmux_decoration_plugin_api::decoration_state::ScriptAccessSpec>,
    generation: u64,
) {
    let Some(access) = access else {
        return;
    };
    state.script_event_subscriptions = access
        .state_channels
        .iter()
        .chain(access.event_channels.iter())
        .cloned()
        .collect();
    for kind in &access.state_channels {
        spawn_script_state_subscription(shared.clone(), state, kind, generation);
    }
    for kind in &access.event_channels {
        spawn_script_broadcast_subscription(shared.clone(), kind, generation);
    }
}

fn plugin_event_kind_from_string(kind: String) -> bmux_plugin_sdk::PluginEventKind {
    bmux_plugin_sdk::PluginEventKind::from_owned(kind)
}

fn spawn_local_current_thread_runtime(
    label: &'static str,
    task: impl Future<Output = ()> + Send + 'static,
) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            tracing::error!("{label} FAILED to build tokio runtime");
            return;
        };
        rt.block_on(task);
    });
}

fn spawn_script_state_subscription(
    shared: SharedState,
    state: &mut State,
    kind: &str,
    generation: u64,
) {
    let event_kind = plugin_event_kind_from_string(kind.to_string());
    let Ok((initial, mut rx)) = bmux_plugin::global_event_bus().subscribe_state_json(&event_kind)
    else {
        tracing::debug!(
            kind,
            "script state subscription skipped; channel unavailable"
        );
        return;
    };
    enqueue_script_json_event_direct(state, initial.as_ref(), true, generation);
    let host_async_handle = shared.host_async_handle();
    let task = async move {
        while rx.changed().await.is_ok() {
            let event = rx.borrow().clone();
            if !send_engine_fire_and_forget(
                &shared,
                DecorationEngineCommand::ScriptJsonEvent {
                    event: event.as_ref().clone(),
                    snapshot: false,
                    generation,
                    reply: None,
                },
            ) {
                break;
            }
        }
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.script_state_subscriber", task);
    } else {
        spawn_local_current_thread_runtime("script state subscriber", task);
    }
}

fn spawn_script_broadcast_subscription(shared: SharedState, kind: &str, generation: u64) {
    let event_kind = plugin_event_kind_from_string(kind.to_string());
    let Ok(mut rx) = bmux_plugin::global_event_bus().subscribe_json(&event_kind) else {
        tracing::debug!(
            kind,
            "script event subscription skipped; channel unavailable"
        );
        return;
    };
    let host_async_handle = shared.host_async_handle();
    let task = async move {
        while let Ok(event) = rx.recv().await {
            if !send_engine_fire_and_forget(
                &shared,
                DecorationEngineCommand::ScriptJsonEvent {
                    event: event.as_ref().clone(),
                    snapshot: false,
                    generation,
                    reply: None,
                },
            ) {
                break;
            }
        }
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.script_event_subscriber", task);
    } else {
        spawn_local_current_thread_runtime("script event subscriber", task);
    }
}

#[cfg(test)]
fn enqueue_script_json_event(
    state: &SharedState,
    event: &bmux_plugin::JsonPluginEvent,
    snapshot: bool,
    generation: u64,
) -> bool {
    send_engine_command_blocking(state, |reply| DecorationEngineCommand::ScriptJsonEvent {
        event: event.clone(),
        snapshot,
        generation,
        reply: Some(reply),
    })
    .unwrap_or(false)
}

fn enqueue_script_json_event_direct(
    state: &mut State,
    event: &bmux_plugin::JsonPluginEvent,
    snapshot: bool,
    generation: u64,
) -> bool {
    let has_component_backend = state
        .script_components
        .values()
        .any(|component| component.backend.is_some());
    if state.script_subscription_generation != generation
        || (state.script_backend.is_none() && !has_component_backend)
    {
        return false;
    }
    if state.script_events.len() >= MAX_SCRIPT_EVENT_QUEUE {
        state.script_events.pop_front();
    }
    let source = event.interface.as_str().to_string();
    let delivery = match event.delivery {
        bmux_plugin::DeliveryMode::Broadcast => ScriptEventDelivery::Broadcast,
        bmux_plugin::DeliveryMode::State => ScriptEventDelivery::State,
    };
    let event_message = ScriptEventMessage {
        source: source.clone(),
        kind: source,
        delivery,
        payload: event.payload.clone(),
        snapshot,
    };
    state.script_events.push_back(event_message.clone());
    for component in state.script_components.values_mut() {
        if component.script_events.len() >= MAX_SCRIPT_EVENT_QUEUE {
            component.script_events.pop_front();
        }
        component.script_events.push_back(event_message.clone());
    }
    publish_scene_if_changed(state);
    true
}

/// The decoration plugin's concrete implementation.
#[derive(Default)]
pub struct DecorationPlugin {
    state: SharedState,
    lifecycle_context: Option<NativeLifecycleContext>,
}

impl DecorationPlugin {
    /// Construct a fresh decoration plugin with the built-in ASCII default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: SharedState::new(),
            lifecycle_context: None,
        }
    }

    /// Build a decoration scene from the plugin's current state.
    ///
    /// The scene covers every pane that has been touched by an
    /// explicit `set_pane_border` call; untouched panes fall through
    /// to the default style and core paints nothing around them.
    #[must_use]
    pub fn build_scene(&self) -> DecorationScene {
        self.state.read_model(|model| model.scene.clone())
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
    type Contract = bmux_decoration_plugin_api::Contract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        if let Some(timeout) = startup_ready_gate_timeout(context.settings.as_ref()) {
            bmux_plugin::register_startup_ready_gate(
                &context.plugin_id,
                SCENE_PUBLISHED_SIGNAL,
                timeout,
            );
        }
        self.lifecycle_context = Some(context);
        // Register the retained scene channel before any mutator (including
        // the initial publication below) tries to publish. Failure is
        // non-fatal — the channel may already exist from a prior load;
        // scene publication tolerates a missing channel.
        let _ = bmux_plugin::global_event_bus()
            .register_state_channel::<bmux_scene_protocol::scene_protocol::DecorationScene>(
                bmux_scene_protocol::scene_protocol::STATE_KIND,
                empty_scene(),
            );
        ensure_decoration_engine(&self.state);
        let summary_theme_loaded = self.state.read_model(|model| model.current_theme.is_some());
        let summary_script_loaded = false;
        let _ = send_engine_command_blocking(&self.state, |reply| {
            DecorationEngineCommand::PublishInitialScene { reply }
        });
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
        spawn_windows_pane_event_subscriber(self.state.clone());
        // Spawn the pane-runtime focus-state subscriber. Unlike the
        // broadcast subscriber above, this one is race-free: the
        // event bus replays the most recently published
        // `SessionFocusStateMap` to late subscribers before any live
        // updates arrive.
        spawn_pane_runtime_focus_state_subscriber(self.state.clone());
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
        spawn_attach_layout_subscriber(self.state.clone());
        spawn_visual_projection_subscriber(self.state.clone());
        Ok(EXIT_OK)
    }

    fn activate_with_async(
        &mut self,
        context: NativeLifecycleContext,
        async_handle: HostAsyncHandle,
    ) -> Result<i32, PluginCommandError> {
        self.state.set_host_async_handle(async_handle);
        self.activate(context)
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let handle = Arc::new(DecorationServiceHandle::new(self.state.clone()));
        let state_service: Arc<dyn DecorationStateService + Send + Sync> = handle.clone();
        let command_service: Arc<dyn DecorationCommandsService + Send + Sync> = handle;
        let _ = bmux_decoration_plugin_api::decoration_state::register_provider(
            registry,
            state_service,
        );
        let _ = bmux_decoration_plugin_api::decoration_commands::register_provider(
            registry,
            command_service,
        );
    }

    #[allow(clippy::too_many_lines)] // route_service! naturally spans every typed op; splitting hurts readability.
    fn invoke_service(
        &self,
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
        let state = self.state.clone();
        bmux_plugin_sdk::route_service!(context, {
            "decoration-state", "pane-decoration" => |req: PaneDecorationArgs, _ctx| {
                let result = state.read_model(|model| Some(pane_decoration_from_read_model(model, req.pane_id)));
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(result)
            },
            "decoration-state", "default-border-style" => |_req: (), _ctx| {
                let border = state.read_model(|model| model.default_border);
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(border)
            },
            "decoration-state", "scene-snapshot" => |_req: (), _ctx| {
                let scene = state.read_model(|model| model.scene.clone());
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(scene)
            },
            "decoration-state", "pane-geometry" => |req: PaneGeometryArgs, _ctx| {
                let geom = state.read_model(|model| geometry_for_pane_read_model(model, req.pane_id));
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(geom)
            },
            "decoration-state", "pane-activity" => |req: PaneActivityArgs, _ctx| {
                let activity = state.read_model(|model| model.activity.get(&req.pane_id).cloned());
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(activity)
            },
            "decoration-state", "current-theme-extension" => |_req: (), _ctx| {
                let theme = state.read_model(|model| model.current_theme.clone());
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(theme)
            },
            "decoration-state", "validate-theme-extension" => |req: ValidateThemeExtensionArgs, _ctx| {
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(validate_theme_extension_toml(&req.toml))
            },
            "decoration-commands", "set-pane-border" => |req: SetPaneBorderArgs, _ctx| {
                let outcome = send_engine_command_blocking(&state, |reply| {
                    DecorationEngineCommand::SetPaneBorder {
                        pane_id: req.pane_id,
                        border: req.border,
                        reply,
                    }
                })
                .unwrap_or_else(|| Err(SetStyleError::StyleUnsupported {
                    style: "decoration engine unavailable".to_string(),
                }));
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(outcome)
            },
            "decoration-commands", "set-default-border" => |req: SetDefaultBorderArgs, _ctx| {
                let outcome = send_engine_command_blocking(&state, |reply| {
                    DecorationEngineCommand::SetDefaultBorder {
                        border: req.border,
                        reply,
                    }
                })
                .unwrap_or_else(|| Err(SetStyleError::StyleUnsupported {
                    style: "decoration engine unavailable".to_string(),
                }));
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(outcome)
            },
            "decoration-commands", "apply-theme-extension" => |req: ApplyThemeExtensionArgs, ctx| {
                let candidates = req
                    .config_dir_candidates
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                let script_host_access = script_host_access_from_context(ctx);
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(apply_theme_extension_toml(
                    &state,
                    &req.toml,
                    &candidates,
                    &script_host_access,
                ))
            },
            "theme-extension", "apply" => |req: ApplyThemeExtensionArgs, ctx| {
                let candidates = req
                    .config_dir_candidates
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<Vec<_>>();
                let script_host_access = script_host_access_from_context(ctx);
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(apply_theme_extension_toml(
                    &state,
                    &req.toml,
                    &candidates,
                    &script_host_access,
                ))
            },
            "decoration-commands", "notify-pane-event" => |req: NotifyPaneEventArgs, _ctx| {
                let outcome = send_engine_command_blocking(&state, |reply| {
                    DecorationEngineCommand::NotifyPaneEvent {
                        event: req.event.clone(),
                        reply,
                    }
                })
                .unwrap_or_else(|| Err(NotifyError::InvalidArgument {
                    reason: "decoration engine unavailable".to_string(),
                }));
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(outcome)
            },
            "decoration-input-hooks", "handle-input" => |req: AttachInputEvent, _ctx| {
                let result = send_engine_command_blocking(&state, |reply| {
                    DecorationEngineCommand::AttachInput {
                        event: req.clone(),
                        reply,
                    }
                })
                .unwrap_or_default();
                Ok::<_, bmux_plugin_sdk::ServiceResponse>(result)
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

fn install_script_components(
    state: &mut State,
    extension: &DecorationThemeExtension,
    config_dir_candidates: &[PathBuf],
    host_access: &ScriptHostAccess,
) {
    state.script_components.clear();
    let Some(components) = extension.components.as_ref() else {
        return;
    };
    let mut instances = BTreeMap::<String, CompiledScriptInstance>::new();
    for (id, spec) in components {
        let instance_id = component_script_instance_id(id, spec);
        if !component_enabled(spec) {
            state.script_components.insert(
                id.clone(),
                empty_script_component_runtime(id, spec, instance_id),
            );
            continue;
        }
        let script = spec
            .script
            .as_deref()
            .and_then(|script| resolve_decoration_script(config_dir_candidates, script));
        let instance_key = component_script_instance_key(&instance_id, script.as_ref());
        let instance = instances
            .entry(instance_key)
            .or_insert_with(|| compile_script_component_instance(id, script, host_access.clone()));
        let runtime = ScriptComponentRuntime {
            id: id.clone(),
            spec: spec.clone(),
            instance_id,
            backend: instance.backend.clone(),
            script_path: instance.script_path.clone(),
            script_source_hash: instance.script_source_hash,
            script_started_at: instance.script_started_at,
            script_frame: 0,
            script_perf: instance
                .script_path
                .clone()
                .map(|path| PerfTracker::new(path, crate::scripting::DEFAULT_WARN_MS)),
            script_events: VecDeque::new(),
        };
        state.script_components.insert(id.clone(), runtime);
    }
}

fn component_script_instance_id(id: &str, spec: &DecorationComponentSpec) -> String {
    spec.script_instance
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(id)
        .to_string()
}

fn component_script_instance_key(instance_id: &str, script: Option<&ResolvedScript>) -> String {
    let Some(script) = script else {
        return format!("{instance_id}:<no-script>");
    };
    format!(
        "{}:{}:{}",
        instance_id,
        script.path.display(),
        script_source_hash(&script.path, &script.source)
    )
}

fn empty_script_component_runtime(
    id: &str,
    spec: &DecorationComponentSpec,
    instance_id: String,
) -> ScriptComponentRuntime {
    ScriptComponentRuntime {
        id: id.to_string(),
        spec: spec.clone(),
        instance_id,
        backend: None,
        script_path: None,
        script_source_hash: None,
        script_started_at: None,
        script_frame: 0,
        script_perf: None,
        script_events: VecDeque::new(),
    }
}

fn compile_script_component_instance(
    component_id: &str,
    script: Option<ResolvedScript>,
    host_access: ScriptHostAccess,
) -> CompiledScriptInstance {
    let Some(script) = script else {
        return CompiledScriptInstance {
            backend: None,
            script_path: None,
            script_source_hash: None,
            script_started_at: None,
        };
    };
    let source_hash = script_source_hash(&script.path, &script.source);
    let script_path = script.path.clone();
    let backend = crate::scripting::make_backend(host_access);
    if !backend.is_functional() {
        tracing::warn!(
            target: "decoration.script",
            component_id,
            script = ?script.path,
            "decoration scripting is not compiled into this build — component script will be ignored",
        );
        return CompiledScriptInstance {
            backend: None,
            script_path: Some(script_path),
            script_source_hash: None,
            script_started_at: None,
        };
    }
    if let Err(error) = backend.compile(&script.path, &script.source) {
        tracing::warn!(
            target: "decoration.script",
            component_id,
            script = ?script.path,
            error = %error,
            "decoration component script failed to compile — component will be ignored",
        );
        return CompiledScriptInstance {
            backend: None,
            script_path: Some(script_path),
            script_source_hash: None,
            script_started_at: None,
        };
    }
    CompiledScriptInstance {
        backend: Some(Arc::from(backend)),
        script_path: Some(script_path),
        script_source_hash: Some(source_hash),
        script_started_at: Some(Instant::now()),
    }
}

/// Compile `script` into a fresh backend and install it on `state`.
/// Invoked during `activate` before the first scene publication so the
/// initial published scene already reflects any script output.
///
/// Failure modes (compile error, stub backend when no `scripting-*`
/// feature is compiled in) are logged at `warn` and leave the plugin
/// in its non-scripted state — the rest of the decoration pipeline
/// keeps working.
fn install_script_backend(
    state: &mut State,
    script: Option<ResolvedScript>,
    host_access: ScriptHostAccess,
) {
    state.script_subscription_generation = state.script_subscription_generation.saturating_add(1);
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
        && host_access.service_grants.is_empty()
    {
        tracing::debug!(
            script = ?script.path,
            "decoration script unchanged; preserving existing backend",
        );
        return;
    }
    let backend = crate::scripting::make_backend(host_access);
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

/// Subscribe to the windows plugin's `pane-event` topic on the typed
/// event bus and reflect updates into the decoration state store.
/// Silently no-ops when the bus channel hasn't been registered yet
/// (e.g. the windows plugin is not loaded, or activates later than
/// the decoration plugin — the bus does not buffer missed events).
fn spawn_windows_pane_event_subscriber(state: SharedState) {
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
    let host_async_handle = state.host_async_handle();
    let task = async move {
        while let Ok(event) = rx.recv().await {
            let event = translate_windows_event(&event);
            if !send_engine_fire_and_forget(
                &state,
                DecorationEngineCommand::PaneEvent(event.clone()),
            ) {
                break;
            }
        }
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.windows_pane_event_subscriber", task);
    } else {
        spawn_local_current_thread_runtime("windows pane event subscriber", task);
    }
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
/// known pane gets `focused = false`. Scene publication runs when the
/// snapshot changes so downstream consumers (attach renderer) pick up
/// the change.
fn spawn_pane_runtime_focus_state_subscriber(state: SharedState) {
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
    let _ = send_engine_fire_and_forget(
        &state,
        DecorationEngineCommand::FocusSnapshot(initial.as_ref().clone()),
    );
    let host_async_handle = state.host_async_handle();
    let task = async move {
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow().clone();
            tracing::trace!(
                entries = snapshot.entries.len(),
                revision = snapshot.revision,
                "focus-state update"
            );
            if !send_engine_fire_and_forget(
                &state,
                DecorationEngineCommand::FocusSnapshot(snapshot.as_ref().clone()),
            ) {
                break;
            }
        }
        tracing::debug!("focus-state subscriber loop exited");
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.focus_state_subscriber", task);
    } else {
        spawn_local_current_thread_runtime("focus-state subscriber", task);
    }
}

/// Reconcile the decoration plugin's `state.activity` map against a
/// pane-runtime focus snapshot. Any pane listed as a focused pane in
/// the snapshot is marked `focused = true`; all other known panes are
/// unfocused. The scene publishes when anything changes.
fn apply_focus_state_map(
    state: &mut State,
    snapshot: &bmux_pane_runtime_plugin_api::pane_runtime_focus::SessionFocusStateMap,
) -> bool {
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
        publish_scene_if_changed(state);
    }
    changed
}

/// Subscribe to the attach-layout state channel and reconcile
/// incoming snapshots into `state.geometry`. Each snapshot carries
/// the set of visible attach surfaces; we insert/update the
/// `PaneGeometry` for every pane-backed surface and drop any panes
/// that disappeared from the new snapshot. Publishing after a change lets
/// subscribers pick up the updated paint commands on the next frame.
fn spawn_attach_layout_subscriber(state: SharedState) {
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
    let _ = send_engine_fire_and_forget(
        &state,
        DecorationEngineCommand::AttachLayoutSnapshot(initial.as_ref().clone()),
    );
    let host_async_handle = state.host_async_handle();
    let task = async move {
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow().clone();
            tracing::trace!(
                surfaces = snapshot.surfaces.len(),
                revision = snapshot.revision,
                "attach-layout update"
            );
            if !send_engine_fire_and_forget(
                &state,
                DecorationEngineCommand::AttachLayoutSnapshot(snapshot.as_ref().clone()),
            ) {
                break;
            }
        }
        tracing::debug!("attach-layout subscriber loop exited");
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.attach_layout_subscriber", task);
    } else {
        spawn_local_current_thread_runtime("attach-layout subscriber", task);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VisualProjectionBatch {
    entries: Vec<VisualProjectionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualProjectionEntry {
    request_id: String,
    surface_id: Uuid,
    pane_id: Uuid,
    encoding: String,
    payload: Arc<[u8]>,
}

fn decode_visual_projection_batch(
    bytes: &[u8],
) -> Result<VisualProjectionBatch, bmux_plugin::EventBusBytesError> {
    const MAGIC: &[u8; 4] = b"BVPB";
    const VERSION: u16 = 1;
    if bytes.len() < 8 || &bytes[0..4] != MAGIC {
        return Err(bmux_plugin::EventBusBytesError::Decode(
            "invalid visual projection batch envelope".to_string(),
        ));
    }
    let version = read_visual_u16(bytes, 4)?;
    if version != VERSION {
        return Err(bmux_plugin::EventBusBytesError::Decode(format!(
            "unsupported visual projection batch version: {version}"
        )));
    }
    let entry_count = usize::from(read_visual_u16(bytes, 6)?);
    let mut offset = 8;
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let request_len = usize::from(read_visual_u16(bytes, offset)?);
        offset += 2;
        let encoding_len = usize::from(read_visual_u16(bytes, offset)?);
        offset += 2;
        let payload_len = usize::try_from(read_visual_u32(bytes, offset)?).map_err(|_| {
            bmux_plugin::EventBusBytesError::Decode("payload length overflow".to_string())
        })?;
        offset += 4;
        let surface_id = read_visual_uuid(bytes, offset)?;
        offset += 16;
        let pane_id = read_visual_uuid(bytes, offset)?;
        offset += 16;
        let request_end = offset.checked_add(request_len).ok_or_else(|| {
            bmux_plugin::EventBusBytesError::Decode("visual request id length overflow".to_string())
        })?;
        let encoding_end = request_end.checked_add(encoding_len).ok_or_else(|| {
            bmux_plugin::EventBusBytesError::Decode("visual encoding length overflow".to_string())
        })?;
        let payload_end = encoding_end.checked_add(payload_len).ok_or_else(|| {
            bmux_plugin::EventBusBytesError::Decode("visual payload length overflow".to_string())
        })?;
        if payload_end > bytes.len() {
            return Err(bmux_plugin::EventBusBytesError::Decode(
                "truncated visual projection batch envelope".to_string(),
            ));
        }
        let request_id = std::str::from_utf8(&bytes[offset..request_end])
            .map_err(|err| bmux_plugin::EventBusBytesError::Decode(err.to_string()))?
            .to_string();
        let encoding = std::str::from_utf8(&bytes[request_end..encoding_end])
            .map_err(|err| bmux_plugin::EventBusBytesError::Decode(err.to_string()))?
            .to_string();
        entries.push(VisualProjectionEntry {
            request_id,
            surface_id,
            pane_id,
            encoding,
            payload: Arc::from(&bytes[encoding_end..payload_end]),
        });
        offset = payload_end;
    }
    if offset != bytes.len() {
        return Err(bmux_plugin::EventBusBytesError::Decode(
            "trailing bytes in visual projection batch envelope".to_string(),
        ));
    }
    Ok(VisualProjectionBatch { entries })
}

fn read_visual_u16(bytes: &[u8], offset: usize) -> Result<u16, bmux_plugin::EventBusBytesError> {
    let end = offset.saturating_add(2);
    let Some(slice) = bytes.get(offset..end) else {
        return Err(bmux_plugin::EventBusBytesError::Decode(
            "truncated visual u16".to_string(),
        ));
    };
    Ok(u16::from_le_bytes(slice.try_into().map_err(|_| {
        bmux_plugin::EventBusBytesError::Decode("invalid visual u16".to_string())
    })?))
}

fn read_visual_u32(bytes: &[u8], offset: usize) -> Result<u32, bmux_plugin::EventBusBytesError> {
    let end = offset.saturating_add(4);
    let Some(slice) = bytes.get(offset..end) else {
        return Err(bmux_plugin::EventBusBytesError::Decode(
            "truncated visual u32".to_string(),
        ));
    };
    Ok(u32::from_le_bytes(slice.try_into().map_err(|_| {
        bmux_plugin::EventBusBytesError::Decode("invalid visual u32".to_string())
    })?))
}

fn read_visual_uuid(bytes: &[u8], offset: usize) -> Result<Uuid, bmux_plugin::EventBusBytesError> {
    let end = offset.saturating_add(16);
    let Some(slice) = bytes.get(offset..end) else {
        return Err(bmux_plugin::EventBusBytesError::Decode(
            "truncated visual uuid".to_string(),
        ));
    };
    Ok(Uuid::from_bytes(slice.try_into().map_err(|_| {
        bmux_plugin::EventBusBytesError::Decode("invalid visual uuid".to_string())
    })?))
}

fn spawn_visual_projection_subscriber(state: SharedState) {
    let event_kind = plugin_event_kind_from_string(DECORATION_VISUAL_PROJECTION_KIND.to_string());
    let _ = bmux_plugin::global_event_bus().register_state_channel_with_bytes_decoder(
        event_kind.clone(),
        VisualProjectionBatch::default(),
        decode_visual_projection_batch,
    );
    let Ok((initial, mut rx)) =
        bmux_plugin::global_event_bus().subscribe_state::<VisualProjectionBatch>(&event_kind)
    else {
        tracing::warn!(
            kind = DECORATION_VISUAL_PROJECTION_KIND,
            "visual projection subscribe failed"
        );
        return;
    };
    let _ = send_engine_fire_and_forget(
        &state,
        DecorationEngineCommand::VisualProjection(initial.as_ref().clone()),
    );
    let host_async_handle = state.host_async_handle();
    let task = async move {
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow().clone();
            if !send_engine_fire_and_forget(
                &state,
                DecorationEngineCommand::VisualProjection(snapshot.as_ref().clone()),
            ) {
                break;
            }
        }
    };
    if let Some(async_handle) = host_async_handle {
        async_handle.spawn_with_name("decoration.visual_projection_subscriber", task);
    } else {
        spawn_local_current_thread_runtime("visual projection subscriber", task);
    }
}

fn apply_visual_projection_batch(state: &mut State, batch: &VisualProjectionBatch) -> bool {
    let mut changed = false;
    for projection in &batch.entries {
        if projection.request_id.is_empty() {
            continue;
        }
        let surface_key = projection.surface_id.to_string();
        let metadata = serde_json::json!({
            "request_id": projection.request_id,
            "surface_id": projection.surface_id,
            "pane_id": projection.pane_id,
            "encoding": projection.encoding,
            "byte_length": projection.payload.len(),
        });
        let request_surfaces = state
            .visual_projection_surfaces
            .entry(projection.request_id.clone())
            .or_default();
        let previous_surface_metadata =
            request_surfaces.insert(surface_key.clone(), metadata.clone());
        let request_surface_bytes = state
            .visual_projection_surface_bytes
            .entry(projection.request_id.clone())
            .or_default();
        let previous_surface_payload =
            request_surface_bytes.insert(surface_key, Arc::clone(&projection.payload));
        let previous_metadata = state
            .visual_projections
            .insert(projection.request_id.clone(), metadata.clone());
        let previous_payload = state.visual_projection_bytes.insert(
            projection.request_id.clone(),
            Arc::clone(&projection.payload),
        );
        changed |= previous_surface_metadata.as_ref() != Some(&metadata);
        changed |= previous_surface_payload.as_deref() != Some(projection.payload.as_ref());
        changed |= previous_metadata.as_ref() != Some(&metadata);
        changed |= previous_payload.as_deref() != Some(projection.payload.as_ref());
    }
    if changed {
        publish_scene_if_changed(state);
    }
    changed
}

fn retain_visual_projection_surfaces(state: &mut State, seen_surfaces: &BTreeSet<Uuid>) -> bool {
    let seen_surface_keys = seen_surfaces
        .iter()
        .map(Uuid::to_string)
        .collect::<BTreeSet<_>>();
    let before_metadata = state.visual_projection_surfaces.clone();
    let before_bytes = state.visual_projection_surface_bytes.clone();
    state.visual_projection_surfaces.retain(|_, surfaces| {
        surfaces.retain(|surface_id, _| seen_surface_keys.contains(surface_id));
        !surfaces.is_empty()
    });
    state.visual_projection_surface_bytes.retain(|_, surfaces| {
        surfaces.retain(|surface_id, _| seen_surface_keys.contains(surface_id));
        !surfaces.is_empty()
    });

    state.visual_projections.clear();
    state.visual_projection_bytes.clear();
    for (request_id, surfaces) in &state.visual_projection_surfaces {
        let Some((surface_id, metadata)) = surfaces.iter().next_back() else {
            continue;
        };
        state
            .visual_projections
            .insert(request_id.clone(), metadata.clone());
        if let Some(bytes) = state
            .visual_projection_surface_bytes
            .get(request_id)
            .and_then(|surface_bytes| surface_bytes.get(surface_id))
        {
            state
                .visual_projection_bytes
                .insert(request_id.clone(), Arc::clone(bytes));
        }
    }

    before_metadata != state.visual_projection_surfaces
        || before_bytes != state.visual_projection_surface_bytes
}

/// Reconcile `state.geometry` against an [`AttachLayoutSnapshot`].
/// Surfaces backed by a pane (non-`None` `pane_id`) update the plugin's
/// geometry record keyed by attach surface id; panes with no remaining visible
/// surfaces are removed. `state.activity` entries for removed panes are cleaned
/// up too so stale focus / zoom flags don't linger.
fn apply_attach_layout_snapshot(
    state: &mut State,
    snapshot: &bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot,
) -> bool {
    use std::collections::BTreeSet;
    let mut changed = false;
    let mut seen_surfaces: BTreeSet<Uuid> = BTreeSet::new();
    let mut visible_surface_order = Vec::new();
    for surface in &snapshot.surfaces {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        seen_surfaces.insert(surface.surface_id);
        visible_surface_order.push(surface.surface_id);
        let new_geometry = PaneGeometry {
            pane_id,
            rect: surface.rect.clone(),
            content_rect: surface.content_rect.clone(),
        };
        let prev = state.geometry.insert(surface.surface_id, new_geometry);
        if prev.as_ref() != state.geometry.get(&surface.surface_id) {
            changed = true;
        }
    }
    if state.visible_surface_order != visible_surface_order {
        state.visible_surface_order = visible_surface_order;
        changed = true;
    }
    // Drop panes that are no longer in the visible set.
    let drop_ids: Vec<Uuid> = state
        .geometry
        .keys()
        .filter(|id| !seen_surfaces.contains(id))
        .copied()
        .collect();
    for surface_id in drop_ids {
        let Some(removed) = state.geometry.remove(&surface_id) else {
            continue;
        };
        if geometry_for_pane(state, removed.pane_id).is_none() {
            state.activity.remove(&removed.pane_id);
            state.panes.remove(&removed.pane_id);
        }
        changed = true;
    }
    changed |= retain_visual_projection_surfaces(state, &seen_surfaces);
    if changed {
        publish_scene_if_changed(state);
    }
    changed
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

fn startup_ready_gate_timeout(settings: Option<&toml::Value>) -> Option<Duration> {
    let Some(table) = settings.and_then(toml::Value::as_table) else {
        return Some(DEFAULT_STARTUP_READY_GATE_TIMEOUT);
    };
    if table
        .get("startup_ready_gate")
        .and_then(toml::Value::as_bool)
        == Some(false)
    {
        return None;
    }
    let timeout = table
        .get("startup_ready_timeout_ms")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .map_or(DEFAULT_STARTUP_READY_GATE_TIMEOUT, Duration::from_millis);
    Some(timeout)
}

/// Re-export the public API types so downstream consumers can import
/// everything from this crate without pulling `bmux_decoration_plugin_api`
/// separately.
pub use bmux_decoration_plugin_api::decoration_state;

/// Canonical interface ids published by this plugin.
pub mod interface_ids {
    pub use bmux_decoration_plugin_api::decoration_commands::INTERFACE_ID as DECORATION_COMMANDS;
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
    assert_eq!(
        bmux_decoration_plugin_api::decoration_state::INTERFACE_ID.as_str(),
        "decoration-state"
    );
    assert_eq!(
        bmux_decoration_plugin_api::decoration_commands::INTERFACE_ID.as_str(),
        "decoration-commands"
    );
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
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::Path;

    struct TestScriptBackend;

    impl ScriptBackend for TestScriptBackend {
        fn compile(
            &self,
            _path: &Path,
            _source: &str,
        ) -> Result<(), crate::scripting::ScriptError> {
            Ok(())
        }

        fn invoke(
            &self,
            _message: &ScriptMessage,
        ) -> Result<crate::scripting::DecorateOutcome, crate::scripting::ScriptError> {
            Ok(crate::scripting::DecorateOutcome {
                surfaces: BTreeMap::new(),
                input_result: None,
                duration: Duration::from_millis(0),
            })
        }

        fn name(&self) -> &'static str {
            "test"
        }

        fn is_functional(&self) -> bool {
            true
        }
    }

    struct RecordingScriptBackend {
        seen: Arc<Mutex<Vec<ScriptMessage>>>,
    }

    impl ScriptBackend for RecordingScriptBackend {
        fn compile(
            &self,
            _path: &Path,
            _source: &str,
        ) -> Result<(), crate::scripting::ScriptError> {
            Ok(())
        }

        fn invoke(
            &self,
            message: &ScriptMessage,
        ) -> Result<crate::scripting::DecorateOutcome, crate::scripting::ScriptError> {
            self.seen.lock().expect("seen lock").push(message.clone());
            Ok(crate::scripting::DecorateOutcome {
                surfaces: BTreeMap::new(),
                input_result: None,
                duration: Duration::from_millis(0),
            })
        }

        fn name(&self) -> &'static str {
            "recording-test"
        }

        fn is_functional(&self) -> bool {
            true
        }
    }

    fn visual_projection_envelope(
        request_id: &str,
        surface_id: Uuid,
        pane_id: Uuid,
        encoding: &str,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BVPB");
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(
            &u16::try_from(request_id.len())
                .expect("request len")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(
            &u16::try_from(encoding.len())
                .expect("encoding len")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("payload len")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(surface_id.as_bytes());
        bytes.extend_from_slice(pane_id.as_bytes());
        bytes.extend_from_slice(request_id.as_bytes());
        bytes.extend_from_slice(encoding.as_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
        let decoration = block_on(handle.pane_decoration(Uuid::nil()))
            .expect("default decoration always present");
        assert_eq!(decoration.border, BorderStyle::Ascii);
        assert_eq!(decoration.pane_id, Uuid::nil());
        assert!(!decoration.focused);
    }

    #[test]
    fn set_pane_border_persists_override() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone());
        let pane = Uuid::from_u128(7);
        let res = block_on(handle.set_pane_border(pane, BorderStyle::Double));
        assert!(res.is_ok());
        let decoration = block_on(handle.pane_decoration(pane)).unwrap();
        assert_eq!(decoration.border, BorderStyle::Double);
    }

    #[test]
    fn set_default_border_changes_global_default() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
    fn publish_scene_if_changed_skips_when_scene_output_is_unchanged() {
        let mut state = State::default();
        let pane = Uuid::from_u128(0xa11);
        state.geometry.insert(
            pane,
            PaneGeometry {
                pane_id: pane,
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 12,
                    h: 5,
                },
                content_rect: Rect {
                    x: 1,
                    y: 1,
                    w: 10,
                    h: 3,
                },
            },
        );
        state.activity.insert(
            pane,
            PaneActivity {
                pane_id: pane,
                focused: false,
                zoomed: false,
                status: PaneLifecycle::Running,
            },
        );

        publish_scene_if_changed(&mut state);
        assert_eq!(state.scene_revision, 1);
        publish_scene_if_changed(&mut state);
        assert_eq!(state.scene_revision, 1);

        state.activity_mut(pane).focused = true;
        publish_scene_if_changed(&mut state);
        assert_eq!(state.scene_revision, 2);
    }

    #[test]
    fn cached_scene_snapshot_does_not_advance_script_frame() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xa12);
        seed_geometry(&plugin, pane, 12, 5);
        plugin.state.with_state(|state| {
            state.script_backend = Some(Box::new(TestScriptBackend));
            publish_scene_if_changed(state);
            assert_eq!(state.script_frame, 1);
        });

        let first = plugin.build_scene();
        let second = plugin.build_scene();

        assert_eq!(first, second);
        let script_frame = plugin.state.with_state(|state| state.script_frame);
        assert_eq!(script_frame, 1, "cached reads must be side-effect free");
    }

    #[test]
    fn direct_noop_style_mutations_do_not_publish_new_revision() {
        let mut state = State::default();
        set_default_border_direct(&mut state, BorderStyle::Single);
        let first_revision = state.scene_revision;

        set_default_border_direct(&mut state, BorderStyle::Single);
        assert_eq!(state.scene_revision, first_revision);

        let pane = Uuid::from_u128(0xa13);
        state.geometry.insert(
            pane,
            PaneGeometry {
                pane_id: pane,
                rect: rect(0, 0, 12, 5),
                content_rect: rect(1, 1, 10, 3),
            },
        );
        set_pane_border_direct(&mut state, pane, BorderStyle::Double);
        let second_revision = state.scene_revision;
        set_pane_border_direct(&mut state, pane, BorderStyle::Double);
        assert_eq!(state.scene_revision, second_revision);
    }

    // Helpers shared by the new theme-aware build-scene tests below.
    // Seeding geometry and activity directly on the shared state lets
    // each test exercise `build_scene` without routing through the
    // full IPC path.
    fn seed_geometry(plugin: &DecorationPlugin, pane: Uuid, w: u16, h: u16) {
        plugin.state.with_state(move |state| {
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
        });
    }

    fn set_activity(plugin: &DecorationPlugin, pane: Uuid, focused: bool, zoomed: bool) {
        plugin.state.with_state(move |state| {
            let entry = state.activity.entry(pane).or_insert(PaneActivity {
                pane_id: pane,
                focused: false,
                zoomed: false,
                status: PaneLifecycle::Running,
            });
            entry.focused = focused;
            entry.zoomed = zoomed;
        });
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
            script_access: None,
            input: None,
            components: None,
        }
    }

    fn install_theme(plugin: &DecorationPlugin, theme: DecorationThemeExtension) {
        plugin.state.with_state(move |state| {
            state.current_theme = Some(theme);
        });
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

    #[test]
    fn theme_extension_parses_script_access() {
        let theme = r##"
            [plugins."bmux.decoration"]
            script = "pulse"

            [plugins."bmux.decoration".unfocused]
            bg = ""
            fg = "#111111"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "single-line"

            [plugins."bmux.decoration".focused]
            bg = ""
            fg = "#ffffff"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "thick"

            [plugins."bmux.decoration".zoomed]
            bg = ""
            fg = "#ffff00"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "double"

            [plugins."bmux.decoration".badges]
            exited = "x"
            running = ">"

            [plugins."bmux.decoration".script_access]
            state_channels = ["third.party/state"]
            event_channels = ["third.party/events"]

            [[plugins."bmux.decoration".script_access.services]]
            capability = "third.party.read"
            kind = "query"
            interface_id = "metrics"
            operation = "pane"
        "##;
        let extension = decoration_extension_from_theme(theme);
        let access = extension.script_access.expect("script access parsed");
        assert_eq!(access.state_channels, vec!["third.party/state"]);
        assert_eq!(access.event_channels, vec!["third.party/events"]);
        assert_eq!(access.services[0].interface_id, "metrics");
    }

    #[test]
    fn script_state_subscription_enqueues_json_snapshot() {
        let plugin = DecorationPlugin::new();
        let kind = format!("test.decoration/state-{}", Uuid::new_v4());
        let event_kind = plugin_event_kind_from_string(kind.clone());
        bmux_plugin::global_event_bus()
            .register_state_channel_with_json_projection::<serde_json::Value>(
                event_kind,
                json!({ "value": 42 }),
            );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = plugin.state.clone();
        let kind_for_subscription = kind.clone();
        plugin.state.with_state({
            let seen = Arc::clone(&seen);
            move |state| {
                state.script_backend = Some(Box::new(RecordingScriptBackend { seen }));
                state.script_subscription_generation = 7;
                let generation = state.script_subscription_generation;
                install_script_event_subscriptions(
                    &shared,
                    state,
                    Some(
                        bmux_decoration_plugin_api::decoration_state::ScriptAccessSpec {
                            state_channels: vec![kind_for_subscription.clone()],
                            event_channels: Vec::new(),
                            services: Vec::new(),
                        },
                    ),
                    generation,
                );
            }
        });
        let seen = seen.lock().expect("seen lock");
        let event = seen
            .iter()
            .find_map(|message| match message {
                ScriptMessage::Event(event) => Some(event),
                ScriptMessage::Input(_) | ScriptMessage::Render(_) => None,
            })
            .expect("snapshot event delivered to script backend");
        assert_eq!(event.source, kind);
        assert_eq!(event.delivery, ScriptEventDelivery::State);
        assert!(event.snapshot);
        assert_eq!(event.payload["value"], 42);
    }

    #[test]
    fn stale_script_subscription_generation_is_ignored() {
        let plugin = DecorationPlugin::new();
        let kind =
            plugin_event_kind_from_string(format!("test.decoration/stale-{}", Uuid::new_v4()));
        plugin.state.with_state(|state| {
            state.script_backend = Some(Box::new(TestScriptBackend));
            state.script_subscription_generation = 2;
        });
        let event = bmux_plugin::JsonPluginEvent {
            interface: kind,
            delivery: bmux_plugin::DeliveryMode::State,
            payload: json!({ "value": "stale" }),
        };
        let accepted = enqueue_script_json_event(&plugin.state, &event, false, 1);
        assert!(!accepted, "stale subscription generation must stop");
        let script_events_empty = plugin
            .state
            .with_state(|state| state.script_events.is_empty());
        assert!(script_events_empty);
    }

    fn install_extension_with_script(
        plugin: &DecorationPlugin,
        extension: DecorationThemeExtension,
    ) {
        let script = extension
            .script
            .as_deref()
            .and_then(|spec| resolve_decoration_script(&[], spec));
        plugin.state.with_state(move |state| {
            state.animation_hz = extension.animation.as_ref().map(|animation| animation.hz);
            install_script_components(state, &extension, &[], &ScriptHostAccess::default());
            state.current_theme = Some(extension);
            install_script_backend(state, script, ScriptHostAccess::default());
        });
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
    fn setting_pane_border_publishes_scene() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone());
        let pane = Uuid::from_u128(42);
        // Seed geometry first — `build_scene` now uses `state.geometry`
        // as the authoritative set of visible panes. Setting an
        // override via `set-pane-border` only affects paint-command
        // selection for panes that also have geometry reported to the
        // plugin.
        plugin.state.with_state(move |state| {
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
        });
        block_on(handle.set_pane_border(pane, BorderStyle::Single)).expect("set");
        let scene = plugin.build_scene();
        assert!(scene.revision >= 1);
        assert!(scene.surfaces.contains_key(&pane));
    }

    #[test]
    fn initial_scene_publication_is_visible() {
        let plugin = DecorationPlugin::new();
        let before = plugin.build_scene().revision;
        assert_eq!(before, 0);
        let _ = send_engine_command_blocking(&plugin.state, |reply| {
            DecorationEngineCommand::PublishInitialScene { reply }
        });
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
        let cap = bmux_plugin_sdk::HostScope::new("bmux.decoration.read").expect("cap");
        let handle = registry
            .get(
                &cap,
                ServiceKind::Query,
                bmux_decoration_plugin_api::decoration_state::INTERFACE_ID.as_str(),
            )
            .expect("handle present");
        let service = handle
            .provider_as_trait::<dyn DecorationStateService + Send + Sync>()
            .expect("downcast");
        let style = block_on(service.default_border_style());
        assert_eq!(style, BorderStyle::default());
    }

    #[test]
    fn startup_ready_gate_settings_default_enabled() {
        assert_eq!(
            startup_ready_gate_timeout(None),
            Some(DEFAULT_STARTUP_READY_GATE_TIMEOUT)
        );
    }

    #[test]
    fn startup_ready_gate_settings_can_disable_gate() {
        let settings = toml::Value::Table(toml::map::Map::from_iter([(
            "startup_ready_gate".to_string(),
            toml::Value::Boolean(false),
        )]));
        assert_eq!(startup_ready_gate_timeout(Some(&settings)), None);
    }

    #[test]
    fn startup_ready_gate_settings_can_override_timeout() {
        let settings = toml::Value::Table(toml::map::Map::from_iter([(
            "startup_ready_timeout_ms".to_string(),
            toml::Value::Integer(75),
        )]));
        assert_eq!(
            startup_ready_gate_timeout(Some(&settings)),
            Some(Duration::from_millis(75))
        );
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
    fn apply_attach_layout_snapshot_caches_rects_and_publishes_scene() {
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
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(state, &snapshot);
        });
        let after = plugin.build_scene().revision;
        assert!(after > before);
        let handle = DecorationServiceHandle::new(plugin.state.clone());
        let geom = block_on(handle.pane_geometry(pane)).expect("geometry cached");
        assert_eq!(geom.rect, rect(0, 0, 20, 5));
        assert_eq!(geom.content_rect, rect(1, 1, 18, 3));
    }

    #[test]
    fn apply_attach_layout_snapshot_skips_publication_for_unchanged_rects() {
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
        let snapshot_for_first = snapshot.clone();
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(state, &snapshot_for_first);
        });
        let r1 = plugin.build_scene().revision;
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(state, &snapshot);
        });
        let r2 = plugin.build_scene().revision;
        assert_eq!(r1, r2, "unchanged geometry must not publish a new revision");
    }

    #[test]
    fn pane_event_focused_updates_activity_and_override() {
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let snapshot_for_apply = snapshot.clone();
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(state, &snapshot_for_apply);
        });
        block_on(handle.notify_pane_event(PaneEvent::Focused { pane_id: pane })).expect("focus");
        // Empty snapshot — pane disappears from the attach layout and
        // the decoration plugin drops all state for it.
        let empty = AttachLayoutSnapshot {
            surfaces: Vec::new(),
            revision: 2,
        };
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(state, &empty);
        });
        assert!(block_on(handle.pane_geometry(pane)).is_none());
        assert!(block_on(handle.pane_activity(pane)).is_none());
        let deco = block_on(handle.pane_decoration(pane)).expect("default");
        assert!(!deco.focused);
    }

    #[test]
    fn build_scene_keys_floating_decoration_by_surface_id() {
        use bmux_attach_layout_protocol::attach_layout_protocol::{
            AttachLayoutSnapshot, AttachSurfaceSummary,
        };
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(699);
        let surface_id = Uuid::from_u128(700);
        let snapshot = AttachLayoutSnapshot {
            surfaces: vec![AttachSurfaceSummary {
                surface_id,
                pane_id: Some(pane),
                rect: rect(2, 3, 20, 5),
                content_rect: rect(3, 4, 18, 3),
                visible: true,
            }],
            revision: 1,
        };
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(state, &snapshot);
        });
        let scene = plugin.build_scene();
        assert!(!scene.surfaces.contains_key(&pane));
        let surface = scene
            .surfaces
            .get(&surface_id)
            .expect("floating surface decoration is keyed by surface id");
        assert_eq!(surface.surface_id, surface_id);
        assert_eq!(surface.rect, rect(2, 3, 20, 5));
        assert_eq!(surface.content_rect, rect(3, 4, 18, 3));
        assert!(
            surface
                .paint_commands
                .iter()
                .any(|cmd| matches!(cmd, PaintCommand::BoxBorder { .. }))
        );
    }

    #[test]
    fn build_scene_includes_geometry_when_pane_has_override_and_layout() {
        use bmux_attach_layout_protocol::attach_layout_protocol::{
            AttachLayoutSnapshot, AttachSurfaceSummary,
        };
        let plugin = DecorationPlugin::new();
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(state, &snapshot);
        });
        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface present");
        assert_eq!(surface.rect, rect(2, 3, 20, 5));
        assert_eq!(surface.content_rect, rect(3, 4, 18, 3));
        assert!(surface.paint_commands.iter().any(|cmd| {
            matches!(
                cmd,
                PaintCommand::BoxBorder {
                    glyphs: BorderGlyphs::SingleLine,
                    ..
                }
            )
        }));
    }

    #[test]
    fn event_bus_focus_change_updates_scene_paint_commands() {
        use bmux_attach_layout_protocol::attach_layout_protocol::{
            AttachLayoutSnapshot, AttachSurfaceSummary,
        };
        use bmux_windows_plugin_api::windows_events::PaneEvent as WindowsPaneEvent;

        let bus = bmux_plugin::global_event_bus();
        bus.register_channel::<WindowsPaneEvent>(
            bmux_windows_plugin_api::windows_events::EVENT_KIND,
        );

        let plugin = DecorationPlugin::new();
        spawn_windows_pane_event_subscriber(plugin.state.clone());
        let pane = Uuid::from_u128(701);
        plugin.state.with_state(move |state| {
            apply_attach_layout_snapshot(
                state,
                &AttachLayoutSnapshot {
                    surfaces: vec![AttachSurfaceSummary {
                        surface_id: pane,
                        pane_id: Some(pane),
                        rect: rect(2, 3, 20, 5),
                        content_rect: rect(3, 4, 18, 3),
                        visible: true,
                    }],
                    revision: 1,
                },
            );
        });

        bus.emit(
            &bmux_windows_plugin_api::windows_events::EVENT_KIND,
            WindowsPaneEvent::Focused { pane_id: pane },
        )
        .expect("emit focus event");

        let focused_scene = wait_for_scene(&plugin, |scene| {
            scene.surfaces.get(&pane).is_some_and(|surface| {
                surface
                    .paint_commands
                    .iter()
                    .any(|cmd| matches!(cmd, PaintCommand::BoxBorder { style, .. } if style.bold))
            })
        });
        let surface = focused_scene.surfaces.get(&pane).expect("surface present");
        assert_eq!(surface.content_rect, rect(3, 4, 18, 3));
    }

    fn wait_for_scene<F>(
        plugin: &DecorationPlugin,
        predicate: F,
    ) -> bmux_scene_protocol::scene_protocol::DecorationScene
    where
        F: Fn(&bmux_scene_protocol::scene_protocol::DecorationScene) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let scene = plugin.build_scene();
            if predicate(&scene) {
                return scene;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for retained decoration scene update"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // PR 3: scene-state publication. The decoration plugin publishes a
    // retained `DecorationScene` on the typed event bus every time state
    // mutates; late attach clients hydrate from the current value.
    #[test]
    fn publish_scene_if_changed_publishes_retained_scene_when_channel_registered() {
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let handle = DecorationServiceHandle::new(plugin.state.clone());
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
        let mut state = State::default();
        install_script_backend(
            &mut state,
            Some(ResolvedScript {
                path: PathBuf::from("bundled:test"),
                source: "function decorate(message) return {} end".into(),
            }),
            ScriptHostAccess::default(),
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

    #[test]
    fn install_script_backend_preserves_identical_script_backend() {
        let mut state = State::default();
        let script = ResolvedScript {
            path: PathBuf::from("bundled:test"),
            source: "function decorate(message) return {} end".into(),
        };
        install_script_backend(
            &mut state,
            Some(script.clone()),
            ScriptHostAccess::default(),
        );
        let started_at = state
            .script_started_at
            .expect("initial install records start instant");
        let source_hash = state
            .script_source_hash
            .expect("initial install records source hash");
        state.script_frame = 42;

        install_script_backend(&mut state, Some(script), ScriptHostAccess::default());

        assert_eq!(state.script_started_at, Some(started_at));
        assert_eq!(state.script_source_hash, Some(source_hash));
        assert_eq!(state.script_frame, 42, "frame is preserved");
    }

    #[test]
    fn install_script_backend_discards_on_compile_failure() {
        let mut state = State::default();
        install_script_backend(
            &mut state,
            Some(ResolvedScript {
                path: PathBuf::from("bundled:broken"),
                source: "function decorate(ctx return {}".into(),
            }),
            ScriptHostAccess::default(),
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
        plugin.state.with_state(move |state| {
            install_script_backend(
                state,
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
                ScriptHostAccess::default(),
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
            publish_scene_if_changed(state);
        });
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
    fn visual_projection_envelope_decodes_binary_sidecar() {
        let surface_id = Uuid::from_u128(7);
        let pane_id = Uuid::from_u128(8);
        let bytes = visual_projection_envelope(
            "pong.content-presence",
            surface_id,
            pane_id,
            "presence-bitset-bin-v1",
            &[1, 2, 3, 4],
        );
        let decoded = decode_visual_projection_batch(&bytes).expect("decode projection");
        assert_eq!(decoded.entries.len(), 1);
        let entry = &decoded.entries[0];
        assert_eq!(entry.request_id, "pong.content-presence");
        assert_eq!(entry.surface_id, surface_id);
        assert_eq!(entry.pane_id, pane_id);
        assert_eq!(entry.encoding, "presence-bitset-bin-v1");
        assert_eq!(entry.payload.as_ref(), &[1, 2, 3, 4]);
    }

    #[test]
    fn visual_projection_updates_script_metadata_and_byte_sidecar() {
        let mut state = State::default();
        let surface_id = Uuid::from_u128(7);
        let pane_id = Uuid::from_u128(8);
        let projection = VisualProjectionBatch {
            entries: vec![VisualProjectionEntry {
                request_id: "pong.content-presence".to_string(),
                surface_id,
                pane_id,
                encoding: "presence-bitset-bin-v1".to_string(),
                payload: Arc::<[u8]>::from([0, 1, 2, 3]),
            }],
        };
        apply_visual_projection_batch(&mut state, &projection);
        let visual = script_visual_payload(&state);
        let metadata = &visual["pong.content-presence"];
        assert_eq!(metadata["request_id"], json!("pong.content-presence"));
        assert_eq!(metadata["surface_id"], json!(surface_id));
        assert_eq!(metadata["pane_id"], json!(pane_id));
        assert_eq!(metadata["encoding"], json!("presence-bitset-bin-v1"));
        assert_eq!(metadata["byte_length"], json!(4));
        assert_eq!(
            metadata["surfaces"][surface_id.to_string()]["encoding"],
            json!("presence-bitset-bin-v1")
        );
        assert_eq!(
            script_visual_bytes_payload(&state)["pong.content-presence"].as_ref(),
            &[0, 1, 2, 3]
        );
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

        plugin.state.with_state(|state| {
            assert!(state.script_backend.is_some(), "script backend installed");
            assert_eq!(
                state.script_path.as_deref(),
                Some(Path::new("bundled:pulse"))
            );
        });

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

        plugin.state.with_state(|state| {
            let component = state
                .script_components
                .get("snake")
                .expect("snake component installed");
            assert!(component.backend.is_some(), "component backend installed");
            assert_eq!(
                component.script_path.as_deref(),
                Some(Path::new("bundled:rainbow_snake")),
            );
        });

        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface emitted");
        let has_snake_text = surface
            .paint_commands
            .iter()
            .any(|cmd| matches!(cmd, PaintCommand::Text { text, .. } if text == "◆"));
        assert!(has_snake_text, "rainbow snake text command emitted");
    }

    #[test]
    fn component_target_filters_render_payload_to_unfocused_panes() {
        let plugin = DecorationPlugin::new();
        let focused = Uuid::from_u128(0xf100);
        let unfocused = Uuid::from_u128(0xf101);
        seed_geometry(&plugin, focused, 20, 5);
        seed_geometry(&plugin, unfocused, 20, 5);
        set_activity(&plugin, focused, true, false);
        set_activity(&plugin, unfocused, false, false);

        let mut theme = sample_theme();
        theme.animation = Some(
            bmux_decoration_plugin_api::decoration_state::AnimationSpec {
                kind: "rainbow_snake".to_string(),
                hz: 12,
            },
        );
        theme.components = Some(BTreeMap::from([(
            "snake".to_string(),
            DecorationComponentSpec {
                enabled: None,
                script: Some("rainbow_snake".to_string()),
                script_instance: None,
                entrypoint: None,
                above: None,
                below: None,
                target: Some(DecorationComponentTargetSpec {
                    kind: "unfocused-panes".to_string(),
                    index: None,
                    modulo: None,
                    remainder: None,
                }),
                animation: None,
                settings: None,
                input: None,
                visual_adapters: None,
            },
        )]));
        install_extension_with_script(&plugin, theme);

        let scene = plugin.build_scene();
        let focused_surface = scene
            .surfaces
            .get(&focused)
            .expect("focused surface emitted");
        let unfocused_surface = scene
            .surfaces
            .get(&unfocused)
            .expect("unfocused surface emitted");
        let focused_has_snake = focused_surface
            .paint_commands
            .iter()
            .any(|cmd| matches!(cmd, PaintCommand::Text { text, .. } if text == "◆"));
        let unfocused_has_snake = unfocused_surface
            .paint_commands
            .iter()
            .any(|cmd| matches!(cmd, PaintCommand::Text { text, .. } if text == "◆"));
        assert!(!focused_has_snake, "target excludes focused pane");
        assert!(unfocused_has_snake, "target includes unfocused pane");
    }

    #[test]
    fn decoration_input_spec_publishes_coarse_input_hook() {
        let plugin = DecorationPlugin::new();
        let mut theme = sample_theme();
        theme.components = Some(BTreeMap::from([(
            "demo.input".to_string(),
            DecorationComponentSpec {
                enabled: None,
                script: Some("pong".to_string()),
                script_instance: None,
                entrypoint: None,
                above: None,
                below: None,
                target: None,
                animation: None,
                settings: None,
                input: Some(DecorationInputSpec {
                    mouse: Some(vec!["down".to_string(), "drag".to_string()]),
                    keys: Some(vec!["up".to_string(), "esc".to_string()]),
                    scope: Some("focused-pane".to_string()),
                    priority: Some(42),
                    min_interval_ms: Some(16),
                }),
                visual_adapters: None,
            },
        )]));
        install_theme(&plugin, theme);
        let scene = plugin.build_scene();
        assert_eq!(scene.input_hooks.len(), 1);
        let hook = &scene.input_hooks[0];
        assert_eq!(hook.id, "bmux.decoration.input");
        assert_eq!(hook.priority, 42);
        assert_eq!(hook.filter.mouse_phases, vec!["down", "drag"]);
        assert_eq!(hook.filter.keys, vec!["esc", "up"]);
        assert_eq!(hook.filter.min_interval_ms, 16);
    }

    #[test]
    fn pong_theme_slice_installs_split_components_and_runs_bundled_script() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xf005);
        seed_geometry(&plugin, pane, 30, 8);
        set_activity(&plugin, pane, true, false);

        let theme = include_str!("../../theme-plugin/assets/themes/pong.toml");
        let extension = decoration_extension_from_theme(theme);
        install_extension_with_script(&plugin, extension);

        plugin.state.with_state(|state| {
            for id in ["pong.ball", "pong.paddles", "pong.score"] {
                let component = state
                    .script_components
                    .get(id)
                    .unwrap_or_else(|| panic!("{id} component installed"));
                assert!(component.backend.is_some(), "{id} backend installed");
                assert_eq!(
                    component.script_path.as_deref(),
                    Some(Path::new("bundled:pong"))
                );
                assert_eq!(component.instance_id, "pong");
            }
            let ball = state
                .script_components
                .get("pong.ball")
                .and_then(|component| component.backend.as_ref())
                .expect("ball backend");
            let paddles = state
                .script_components
                .get("pong.paddles")
                .and_then(|component| component.backend.as_ref())
                .expect("paddles backend");
            let score = state
                .script_components
                .get("pong.score")
                .and_then(|component| component.backend.as_ref())
                .expect("score backend");
            assert!(Arc::ptr_eq(ball, paddles));
            assert!(Arc::ptr_eq(ball, score));
        });

        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface emitted");
        assert!(
            surface
                .before_content_paint_commands
                .iter()
                .any(|cmd| matches!(cmd, PaintCommand::Text { text, .. } if text == "●"))
        );
        assert!(surface.paint_commands.iter().any(
            |cmd| matches!(cmd, PaintCommand::Text { text, .. } if text == "▌" || text == "▐")
        ));
        assert!(
            surface
                .paint_commands
                .iter()
                .any(|cmd| matches!(cmd, PaintCommand::Text { text, .. } if text.contains(" : ")))
        );
    }

    #[test]
    fn component_layering_uses_relative_above_below_order() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xf003);
        seed_geometry(&plugin, pane, 20, 5);
        let extension = decoration_extension_from_theme(
            r##"
            [plugins."bmux.decoration".unfocused]
            bg = ""
            fg = "#606060"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "single-line"

            [plugins."bmux.decoration".focused]
            bg = ""
            fg = "#e0e0e0"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "thick"

            [plugins."bmux.decoration".zoomed]
            bg = ""
            fg = "#ffffff"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "double"

            [plugins."bmux.decoration".badges]
            running = ""
            exited = ""

            [plugins."bmux.decoration".components."performance.border"]
            script = "decorations/border.lua"

            [plugins."bmux.decoration".components."performance.header"]
            above = ["snake.body"]
            script = "decorations/header.lua"

            [plugins."bmux.decoration".components."snake.body"]
            above = ["performance.border"]
            below = ["performance.header"]
            script = "decorations/snake.lua"
            "##,
        );
        let config_dir =
            std::env::temp_dir().join(format!("bmux-components-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(config_dir.join("decorations")).expect("mkdir decorations");
        write_component_test_script(&config_dir, "border", "B");
        write_component_test_script(&config_dir, "snake", "S");
        write_component_test_script(&config_dir, "header", "H");
        plugin.state.with_state(move |state| {
            install_script_components(
                state,
                &extension,
                std::slice::from_ref(&config_dir),
                &ScriptHostAccess::default(),
            );
            state.current_theme = Some(extension);
            publish_scene_if_changed(state);
        });

        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface emitted");
        let ordered_text = surface
            .paint_commands
            .iter()
            .filter_map(|cmd| match cmd {
                PaintCommand::Text { text, .. } if text == "B" || text == "S" || text == "H" => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(ordered_text, vec!["B", "S", "H"]);
    }

    #[test]
    fn pane_content_anchor_splits_component_paint_commands() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(0xf004);
        seed_geometry(&plugin, pane, 20, 5);
        let extension = decoration_extension_from_theme(
            r##"
            [plugins."bmux.decoration".unfocused]
            bg = ""
            fg = "#606060"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "single-line"

            [plugins."bmux.decoration".focused]
            bg = ""
            fg = "#e0e0e0"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "thick"

            [plugins."bmux.decoration".zoomed]
            bg = ""
            fg = "#ffffff"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "double"

            [plugins."bmux.decoration".badges]
            running = ""
            exited = ""

            [plugins."bmux.decoration".components."pong.ball"]
            below = ["pane.content"]
            script = "decorations/ball.lua"

            [plugins."bmux.decoration".components."pong.score"]
            above = ["pane.content"]
            script = "decorations/score.lua"
            "##,
        );
        let config_dir =
            std::env::temp_dir().join(format!("bmux-components-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(config_dir.join("decorations")).expect("mkdir decorations");
        write_component_test_script(&config_dir, "ball", "B");
        write_component_test_script(&config_dir, "score", "S");
        plugin.state.with_state(move |state| {
            install_script_components(
                state,
                &extension,
                std::slice::from_ref(&config_dir),
                &ScriptHostAccess::default(),
            );
            state.current_theme = Some(extension);
            publish_scene_if_changed(state);
        });

        let scene = plugin.build_scene();
        let surface = scene.surfaces.get(&pane).expect("surface emitted");
        assert!(
            surface
                .before_content_paint_commands
                .iter()
                .any(|cmd| matches!(cmd, PaintCommand::Text { text, .. } if text == "B"))
        );
        assert!(
            surface
                .paint_commands
                .iter()
                .any(|cmd| matches!(cmd, PaintCommand::Text { text, .. } if text == "S"))
        );
    }

    fn write_component_test_script(config_dir: &Path, name: &str, label: &str) {
        std::fs::write(
            config_dir.join("decorations").join(format!("{name}.lua")),
            format!(
                r#"
                function decorate(message)
                    if message.kind ~= "render" then
                        return nil
                    end
                    local surfaces = {{}}
                    for _, pane in ipairs(message.panes or {{}}) do
                        surfaces[pane.id] = {{{{
                            kind = "text",
                            col = pane.rect.x,
                            row = pane.rect.y,
                            z = 0,
                            text = "{label}",
                            style = {{}},
                        }}}}
                    end
                    return {{ surfaces = surfaces }}
                end
                "#,
            ),
        )
        .expect("write script");
    }

    #[test]
    fn stale_same_hz_animation_policy_does_not_publish_scene_if_changed() {
        let mut state = State {
            animation_hz: Some(100),
            animation_generation: 2,
            script_backend: Some(Box::new(TestScriptBackend)),
            ..State::default()
        };

        assert!(!run_animation_tick_if_current(
            &mut state,
            AnimationDriverPolicy {
                hz: Some(100),
                generation: 1,
            },
        ));
        assert_eq!(
            state.scene_revision, 0,
            "stale same-hz animation policy must not publish a scene",
        );
    }

    #[test]
    fn script_backend_not_installed_when_theme_has_no_script() {
        let mut state = State::default();
        install_script_backend(&mut state, None, ScriptHostAccess::default());
        assert!(
            state.script_backend.is_none(),
            "install_script_backend must leave no backend when script is None",
        );
    }

    #[test]
    fn install_script_backend_none_clears_existing_backend() {
        let mut state = State::default();
        install_script_backend(
            &mut state,
            Some(ResolvedScript {
                path: PathBuf::from("bundled:test"),
                source: "function decorate(message) return {} end".into(),
            }),
            ScriptHostAccess::default(),
        );
        assert!(state.script_backend.is_some(), "backend installed first");

        install_script_backend(&mut state, None, ScriptHostAccess::default());

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
