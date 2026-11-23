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
//! 1. The plugin exposes a constructor; the attach runtime registers the
//!    resulting `Arc<dyn AttachRenderExtension>` into its own
//!    [`RenderExtensionRegistry`] (there is no process-wide registry).
//! 2. The attach runtime reads that registry on every frame and queries
//!    each extension for each visible surface.
//! 3. Extensions paint through
//!    [`AttachRenderExtension::render_layer_scene_with_context`], which returns
//!    a retained description of the current visual intent. The renderer owns
//!    diffing, stale cleanup, content replay, terminal graphics lifecycle, and
//!    byte emission. Extensions describe what they show; they never write
//!    terminal bytes themselves.
//! 4. An extension that can say when its scene changes reports
//!    [`AttachRenderExtension::render_layer_revision`]; the host then samples
//!    it again only when the revision, rect or context changes.
//! 5. When a surface disappears (pane closed, layout recomputed without
//!    it), the attach runtime calls
//!    [`AttachRenderExtension::surface_removed`] so the extension can
//!    evict any cached plugin state.
//!
//! Extensions are expected to be lightweight: the registry lookup is
//! `O(n)` per render, and scenes are built before every frame whose revision
//! changed (every frame without one). Prefer caching source state when it
//! changes, then constructing a retained scene cheaply.
//!
//! # Retained scene example
//!
//! A retained scene describes *what should be visible now* rather than what
//! changed since the previous frame. Item keys must be stable across frames for
//! the same logical visual element; the renderer uses them for diffing and for
//! terminal graphics identity.
//!
//! ```rust,ignore
//! use bmux_plugin::{
//!     AttachRenderExtension, ExtensionRect, RenderExtensionContext,
//!     RenderExtensionLayer, RenderLayerScene, RenderStyle,
//! };
//! use uuid::Uuid;
//!
//! struct HeaderExtension;
//!
//! impl AttachRenderExtension for HeaderExtension {
//!     fn name(&self) -> &str { "example.header" }
//!
//!     fn render_layer_scene_with_context(
//!         &self,
//!         _surface_id: Uuid,
//!         _surface_rect: &ExtensionRect,
//!         layer: RenderExtensionLayer,
//!         _context: &RenderExtensionContext,
//!     ) -> Option<RenderLayerScene> {
//!         if layer != RenderExtensionLayer::AfterPaneContent {
//!             return Some(RenderLayerScene::builder().build());
//!         }
//!
//!         Some(
//!             RenderLayerScene::builder()
//!                 .revision(42)
//!                 .text("header/title", 10, 1, 0, "running", RenderStyle::new().bold())
//!                 .build(),
//!         )
//!     }
//! }
//! ```
//!
//! ## Writing scenes
//!
//! - Keep damage out of the plugin. Return all current visual items and let the
//!   renderer diff them against its retained snapshot.
//! - Build keyed [`RenderSceneItem`]s via [`RenderLayerScene::builder`]. Use
//!   stable keys such as `"header/title"`, `"score/home"`, or `"border/top"`
//!   instead of frame-local counters.
//! - [`RenderLayerScene::revision`] is for diagnostics; reuse is gated on
//!   [`AttachRenderExtension::render_layer_revision`], and cleanup on retained
//!   item key/bounds/content diffs.
//! - Return `None` to paint nothing on a surface.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

pub use bmux_presentation_scene::paint::{
    BorderGlyphs, ExtensionRect, RenderCell, RenderColor, RenderDamage, RenderExtensionLayer,
    RenderLayerItem, RenderLayerScene, RenderLayerSceneBuilder, RenderNamedColor, RenderOp,
    RenderOpBatchBuilder, RenderSceneItem, RenderSceneItemKey, RenderStyle, RenderTextSpan,
    RenderUnderCell, TerminalGraphicFill, TerminalGraphicOverlay, TerminalRenderCapabilities,
    TerminalRgba, border_outline_rects, clip_render_text_run_to_rect,
    render_char_display_width_u16, render_single_display_cell_char, render_text_width_u16,
};

/// Owning-plugin codec for a remote input endpoint. Local input support types
/// are not a service wire contract: implementations use generated BPDL transport.
pub trait AttachInputCodec: Send + Sync {
    fn endpoint(&self) -> AttachInputEndpoint;

    /// Encode a checked request using the owning interface's generated identity.
    ///
    /// # Errors
    /// Returns an error when the event cannot be represented by this contract.
    fn encode_event(&self, event: &AttachInputEvent) -> Result<Vec<u8>, String>;

    /// Decode a response only after checked provider acceptance is verified.
    ///
    /// # Errors
    /// Rejects unsupported response framing or malformed generated results.
    fn decode_result(&self, payload: &[u8]) -> Result<AttachInputResult, String>;
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct AttachInputEndpoint {
    pub capability: String,
    pub interface_id: String,
    pub operation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputHookFilter {
    pub mouse_phases: Vec<String>,
    pub keys: Vec<String>,
    pub scope: String,
    pub min_interval_ms: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputHook {
    pub id: String,
    pub owner_plugin_id: String,
    pub priority: i16,
    pub endpoint: AttachInputEndpoint,
    pub filter: AttachInputHookFilter,
}

#[allow(clippy::struct_excessive_bools)] // Mirrors terminal modifier flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
    pub hyper: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputPaneContext {
    pub pane_id: Uuid,
    pub surface_id: Uuid,
    pub rect: ExtensionRect,
    pub content_rect: ExtensionRect,
    pub focused: bool,
    /// How the pane's program asked for input, so a hook can tell (for example) whether a
    /// mouse event belongs to the program.
    #[serde(default)]
    pub modes: AttachInputPaneModes,
}

/// How a pane's program asked its terminal to deliver input, as the client's replica of the
/// pane holds it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputPaneModes {
    pub mouse: AttachInputMouseModes,
    pub keys: AttachInputKeyModes,
    /// The program is on the alternate screen (a full-screen application).
    pub alternate_screen: bool,
}

/// The mouse reporting a pane's program enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputMouseModes {
    pub tracking: AttachInputMouseTracking,
    /// Reports use SGR encoding.
    pub sgr: bool,
}

/// Which mouse events a pane's program asked to receive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AttachInputMouseTracking {
    #[default]
    None,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

/// Keyboard and paste modes a pane's program set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputKeyModes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputEvent {
    pub hook_id: String,
    pub event_kind: String,
    pub phase: String,
    pub button: Option<String>,
    pub key: Option<String>,
    pub col: Option<u16>,
    pub row: Option<u16>,
    /// Signed wheel movement for pointer wheel events; zero otherwise.
    #[serde(default)]
    pub wheel_delta: i16,
    /// The pointer's column on the attach viewport, for pointer events.
    /// Unlike `col`, it is not relative to the surface, so a captured drag
    /// that leaves the surface on any side keeps its true position.
    #[serde(default)]
    pub screen_col: Option<u16>,
    /// The pointer's row on the attach viewport, for pointer events.
    #[serde(default)]
    pub screen_row: Option<u16>,
    pub modifiers: AttachInputModifiers,
    pub focused_pane: Option<AttachInputPaneContext>,
    pub hovered_pane: Option<AttachInputPaneContext>,
    /// The interactive region this event is addressed to, when it has one.
    /// Producers resolve `key` against the hit table they published with
    /// `revision`, instead of parsing `hook_id`.
    #[serde(default)]
    pub target: Option<AttachInputTarget>,
}

/// The surface region an input event is addressed to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputTarget {
    /// Owner-local surface id.
    pub surface: String,
    /// Owner-local region id.
    pub region: String,
    /// The surface revision the region was published with.
    pub revision: u64,
    /// The region's typed key, when it was published with one.
    pub key: Option<bmux_presentation_scene::scene::HitKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputServiceInvocation {
    pub endpoint: AttachInputEndpoint,
    /// Optional local continuation for the opaque service response.
    #[serde(default)]
    pub response_endpoint: Option<Box<AttachInputEndpoint>>,
    pub payload: Vec<u8>,
}

#[allow(clippy::struct_excessive_bools)] // Independent routing flags returned by plugin input hooks.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputResult {
    pub consumed: bool,
    /// Consume a gesture without changing its existing keyboard focus target.
    #[serde(default)]
    pub preserve_focus: bool,
    pub capture_pointer: bool,
    pub capture_keyboard: Vec<String>,
    pub release_capture: bool,
    /// Keep a stable-region pointer capture across retained surface revisions.
    /// Presentation-owned gestures use this while redrawing drag feedback.
    #[serde(default)]
    pub preserve_capture_across_revisions: bool,
    pub dirty: bool,
    /// Optional user-facing status emitted by best-effort input hooks.
    ///
    /// This keeps input-hook health reporting generic: overloaded hooks can
    /// explain that they deferred work without failing the input path.
    #[serde(default)]
    pub status_message: Option<String>,
    /// Optional generic service invocation requested by a process-local
    /// presentation handler after updating its retained interaction state.
    #[serde(default)]
    pub service_invocation: Option<AttachInputServiceInvocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachVisualAdapterRequest {
    pub id: String,
    pub adapter: String,
    pub owner_plugin_id: String,
    pub event_kind: String,
    pub scope: String,
    pub area: String,
    pub max_hz: u16,
    pub dirty_only: bool,
    pub max_bytes: u32,
    pub settings: BTreeMap<String, String>,
}

impl AttachVisualAdapterRequest {
    #[must_use]
    pub const fn min_interval_ms(&self) -> u64 {
        if self.max_hz == 0 {
            0
        } else {
            1_000_u64 / self.max_hz as u64
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachVisualProjectionUpdate {
    pub request_id: String,
    pub event_kind: String,
    pub surface_id: Uuid,
    pub pane_id: Uuid,
    pub encoding: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachVisualCellRef<'a> {
    pub text: &'a str,
    pub width: u8,
    pub wide_continuation: bool,
}

pub trait AttachVisualSurfaceView {
    fn surface_id(&self) -> Uuid;
    fn pane_id(&self) -> Uuid;
    fn rect(&self) -> ExtensionRect;
    fn content_rect(&self) -> ExtensionRect;
    fn focused(&self) -> bool;
    fn grid_revision(&self) -> u64;
    fn content_revision(&self) -> u64 {
        self.grid_revision()
    }

    /// Return a cheap row-level content fingerprint when available.
    ///
    /// Fingerprints are scoped to the current surface dimensions and should
    /// change whenever text/wide-cell occupancy in that row changes. Adapters
    /// can use this as an optional fast path to reuse cached per-row work
    /// without pulling full grids through IPC. The default keeps the API
    /// ergonomic for surfaces that only expose cells.
    fn row_content_fingerprint(&self, _row: u16) -> Option<u64> {
        None
    }

    fn width(&self) -> u16;
    fn height(&self) -> u16;
    fn cell(&self, x: u16, y: u16) -> Option<AttachVisualCellRef<'_>>;
}

pub trait AttachVisualFrameView {
    fn surface_count(&self) -> usize;
    fn surface(&self, index: usize) -> Option<&dyn AttachVisualSurfaceView>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachVisualAdapterOutput {
    pub encoding: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachVisualProjectionResult {
    Unchanged,
    Updated(AttachVisualAdapterOutput),
}

pub trait AttachVisualAdapter: Send + Sync {
    fn id(&self) -> &str;

    /// Create optional adapter-owned cache state for one request/surface pair.
    ///
    /// The host stores this value opaquely and passes it back to
    /// [`Self::project_cached`]. Adapters that do not need state can rely on
    /// the default `None`.
    fn new_cache(&self, _request: &AttachVisualAdapterRequest) -> Option<Box<dyn Any + Send>> {
        None
    }

    /// Project a borrowed visual surface into an adapter-owned compact payload.
    ///
    /// # Errors
    ///
    /// Returns an error string when the adapter cannot encode the requested
    /// payload. Callers should treat adapter failures as non-fatal frame work.
    fn project(
        &self,
        surface: &dyn AttachVisualSurfaceView,
        request: &AttachVisualAdapterRequest,
        out: &mut Vec<u8>,
    ) -> Result<AttachVisualAdapterOutput, String>;

    /// Project with optional adapter-owned cache state.
    ///
    /// # Errors
    ///
    /// Returns an error string when the adapter cannot encode the requested
    /// payload. The default implementation preserves compatibility by calling
    /// [`Self::project`].
    fn project_cached(
        &self,
        surface: &dyn AttachVisualSurfaceView,
        request: &AttachVisualAdapterRequest,
        _cache: Option<&mut dyn Any>,
        out: &mut Vec<u8>,
    ) -> Result<AttachVisualAdapterOutput, String> {
        self.project(surface, request, out)
    }

    /// Project with optional cache state and allow adapters to report that the
    /// compact semantic projection did not change.
    ///
    /// # Errors
    ///
    /// Returns an error string when the adapter cannot encode the requested
    /// payload. The default preserves existing adapter ergonomics by wrapping
    /// [`Self::project_cached`] in an updated result.
    fn project_incremental_cached(
        &self,
        surface: &dyn AttachVisualSurfaceView,
        request: &AttachVisualAdapterRequest,
        cache: Option<&mut dyn Any>,
        out: &mut Vec<u8>,
    ) -> Result<AttachVisualProjectionResult, String> {
        self.project_cached(surface, request, cache, out)
            .map(AttachVisualProjectionResult::Updated)
    }
}

/// Generic presentation role supplied to render extensions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RenderSurfaceRole {
    #[default]
    Content,
    Overlay,
}

/// Extension-owned chrome disposition for one generic surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RenderSurfaceChrome {
    #[default]
    Fallback,
    Extension,
}

/// Context supplied to render extensions for one render pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderExtensionContext {
    pub capabilities: TerminalRenderCapabilities,
    pub surface_role: RenderSurfaceRole,
    pub opaque_occluders: Vec<ExtensionRect>,
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

    /// Declare whether this extension owns outer chrome for a generic surface.
    fn surface_chrome_with_context(
        &self,
        _surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        _context: &RenderExtensionContext,
    ) -> RenderSurfaceChrome {
        RenderSurfaceChrome::Fallback
    }

    /// Refresh any retained state that the extension consumes before a
    /// frame is rendered.
    ///
    /// The attach renderer calls this once per frame, before querying
    /// render revisions or scenes. Implementations should make this a
    /// cheap non-blocking drain of already-retained state (for example,
    /// `watch::Receiver::borrow_and_update`), not an IPC call or other
    /// unbounded operation.
    fn refresh_state(&self) {}

    /// Whether this extension painted, or will paint, cells inside `content` of the surface on
    /// `layer`. The host moves a pane's rows on the terminal with a scroll region only when no
    /// extension paints inside its content, since moving the rows would move those cells too.
    /// The default is conservative: an extension that cannot tell is treated as painting there.
    fn paints_inside_content(
        &self,
        _surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        _layer: RenderExtensionLayer,
        _content: &ExtensionRect,
    ) -> bool {
        true
    }

    /// Return an optional revision token for render output on this surface
    /// and layer. The default is `None`: sampled every frame.
    ///
    /// # Contract
    ///
    /// `Some(revision)` is a promise: while the token stays the same, and the
    /// host asks with the same surface rect and [`RenderExtensionContext`],
    /// everything the extension would answer for this surface and layer stays
    /// the same: its scene and what it paints inside the content. The host
    /// relies on it: before each frame it samples the layer only when the
    /// token, the rect or the context changed, and otherwise reuses what it
    /// sampled last (it does not call the layer's render methods at all).
    /// When both of a surface's layers are reused, the surface's chrome and
    /// content-rect override are reused too.
    ///
    /// So an extension that reports a revision must change it whenever any
    /// of that output can change, including for state the host cannot see
    /// (animation, time, focus, a newly arrived scene). A content hash of the
    /// inputs the output is built from is a good token. Return `None` when
    /// that cannot be promised: the host then samples the layer every frame.
    ///
    /// A revision change alone does not imply stale cleanup: the host still
    /// derives cleanup from retained item keys, bounds and contents.
    fn render_layer_revision(
        &self,
        _surface_id: Uuid,
        _layer: RenderExtensionLayer,
    ) -> Option<u64> {
        None
    }

    /// Rules that redact or restyle the text of every pane this client renders: the renderer
    /// applies them to each row it paints from its replica, so masked text never reaches the
    /// host terminal. Return the same `Arc` while the rules are unchanged (a new set makes the
    /// renderer draw panes again). The default, `None`, transforms nothing and costs nothing.
    fn pane_text_rules(&self) -> Option<Arc<crate::PaneTextRules>> {
        None
    }

    /// Return retained declarative visual intent for one layer.
    ///
    /// This is how an extension paints: the scene is everything it shows on
    /// the surface's layer now, and the host renderer owns diffing, stale
    /// cleanup, content replay, z-ordering, terminal graphics lifecycle, and
    /// byte emission. On [`RenderExtensionLayer::BeforePaneContent`], items
    /// land under the pane's content, where content cells with text cover
    /// them. The default, `None`,
    /// paints nothing.
    fn render_layer_scene_with_context(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<RenderLayerScene> {
        let _ = (surface_id, surface_rect, layer, context);
        None
    }

    /// [`Self::render_layer_scene_with_context`] behind a shared pointer.
    ///
    /// The host compares the pointers of consecutive frames' scenes: the same
    /// allocation means the same scene, which it then neither converts nor
    /// diffs again. Implementations that retain scenes across frames (as the
    /// host's own sampled snapshots do) should return the retained `Arc`; the
    /// default wraps a freshly built scene.
    fn render_layer_scene_shared(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<Arc<RenderLayerScene>> {
        self.render_layer_scene_with_context(surface_id, surface_rect, layer, context)
            .map(Arc::new)
    }

    /// Override the surface's content-rect inset. Returning `Some`
    /// tells the attach runtime "the PTY should render inside this
    /// smaller rectangle"; `None` means the extension has no opinion.
    /// When multiple extensions return `Some`, the attach runtime
    /// picks the narrowest inset.
    fn content_rect_override(&self, _surface_id: Uuid) -> Option<ExtensionRect> {
        None
    }

    /// Owning-plugin codec for a remote input endpoint used by this extension.
    /// Host-local presentation handlers do not require a wire codec.
    fn input_codec(&self, _endpoint: &AttachInputEndpoint) -> Option<Arc<dyn AttachInputCodec>> {
        None
    }

    /// Return coarse input hooks currently owned by this extension.
    /// Hooks are subscriptions, not hit-test regions: the host filters cheaply
    /// and the owning extension decides whether a specific event is consumed.
    fn input_hooks(&self) -> Vec<AttachInputHook> {
        Vec::new()
    }

    /// Return visual adapter requests currently owned by this extension.
    /// Requests are coarse subscriptions over the attach-local visual frame;
    /// exact extraction is delegated to plugin-owned visual adapters.
    fn visual_adapter_requests(&self) -> Vec<AttachVisualAdapterRequest> {
        Vec::new()
    }

    /// Observe the borrowed attach-local visual frame and emit compact,
    /// plugin-owned projection updates. Implementations must be non-blocking
    /// and should use registered visual adapters for hot-path extraction.
    fn observe_visual_frame(
        &self,
        _frame: &dyn AttachVisualFrameView,
        _updates: &mut Vec<AttachVisualProjectionUpdate>,
    ) {
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

    /// Resolve an owning plugin's remote-input contract in this registry.
    #[must_use]
    pub fn input_codec(&self, endpoint: &AttachInputEndpoint) -> Option<Arc<dyn AttachInputCodec>> {
        self.snapshot()
            .into_iter()
            .find_map(|extension| extension.input_codec(endpoint))
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

#[derive(Default)]
pub struct VisualAdapterRegistry {
    entries: RwLock<BTreeMap<String, Arc<dyn AttachVisualAdapter>>>,
}

impl VisualAdapterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, adapter: Arc<dyn AttachVisualAdapter>) {
        if let Ok(mut guard) = self.entries.write() {
            guard.insert(adapter.id().to_string(), adapter);
        }
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn AttachVisualAdapter>> {
        self.entries.read().ok()?.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_border_paints_its_outline_and_text_its_width() {
        let pane = ExtensionRect::new(0, 0, 20, 10);
        let content = ExtensionRect::new(1, 1, 18, 8);
        let border = RenderOp::border(pane, BorderGlyphs::ascii(), RenderStyle::default());
        assert!(
            !border
                .painted_rects()
                .iter()
                .any(|rect| rect.intersects(content))
        );
        assert_eq!(border_outline_rects(pane).len(), 4);
        let title = RenderOp::text_run(2, 0, "title", RenderStyle::default());
        assert_eq!(title.painted_rects(), [ExtensionRect::new(2, 0, 5, 1)]);
        let badge = RenderOp::text_run(3, 4, "!", RenderStyle::default());
        assert!(
            badge
                .painted_rects()
                .iter()
                .any(|rect| rect.intersects(content))
        );
    }

    struct RecordingExtension {
        name: String,
        removed: Mutex<Vec<Uuid>>,
    }

    impl AttachRenderExtension for RecordingExtension {
        fn name(&self) -> &str {
            &self.name
        }

        fn surface_removed(&self, surface_id: Uuid) {
            self.removed.lock().unwrap().push(surface_id);
        }
    }

    #[test]
    fn display_cell_helpers_handle_wide_and_sparse_text() {
        assert_eq!(render_text_width_u16("a界"), 3);
        assert_eq!(render_char_display_width_u16('界'), 2);
        assert_eq!(render_single_display_cell_char("x"), Some('x'));
        assert_eq!(render_single_display_cell_char("界"), None);
        assert_eq!(render_single_display_cell_char(""), None);
        assert_eq!(render_single_display_cell_char("ab"), None);
    }

    #[test]
    fn clip_render_text_run_preserves_display_cell_boundaries() {
        assert_eq!(
            clip_render_text_run_to_rect(
                0,
                "界a",
                ExtensionRect {
                    x: 1,
                    y: 0,
                    w: 4,
                    h: 1,
                },
            ),
            Some((2, "a".to_string()))
        );
        assert_eq!(
            clip_render_text_run_to_rect(
                0,
                "界a",
                ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                },
            ),
            Some((0, "界".to_string()))
        );
    }

    #[test]
    fn render_capability_fallbacks_are_deterministic() {
        let capabilities = TerminalRenderCapabilities {
            truecolor: false,
            unicode_box_drawing: false,
            ..TerminalRenderCapabilities::default()
        };
        assert_eq!(
            RenderColor::Rgb { r: 255, g: 0, b: 0 }.for_capabilities(capabilities),
            RenderColor::Indexed(196)
        );
        assert_eq!(
            RenderColor::Rgb {
                r: 128,
                g: 128,
                b: 128,
            }
            .for_capabilities(capabilities),
            RenderColor::Indexed(145)
        );
        assert_eq!(
            BorderGlyphs::rounded().for_capabilities(capabilities),
            BorderGlyphs::ascii()
        );
        assert_eq!(
            RenderColor::Rgb { r: 1, g: 2, b: 3 }
                .for_capabilities(TerminalRenderCapabilities::default()),
            RenderColor::Rgb { r: 1, g: 2, b: 3 }
        );
    }

    #[test]
    fn render_builders_create_expected_ops() {
        let style = RenderStyle::new()
            .named_foreground(RenderNamedColor::BrightCyan)
            .rgb_background(1, 2, 3)
            .bold()
            .underline()
            .italic()
            .reverse()
            .dim()
            .blink()
            .strikethrough();
        assert_eq!(
            style.fg,
            Some(RenderColor::Named(RenderNamedColor::BrightCyan))
        );
        assert_eq!(style.bg, Some(RenderColor::Rgb { r: 1, g: 2, b: 3 }));
        assert!(style.bold);
        assert!(style.underline);
        assert!(style.italic);
        assert!(style.reverse);
        assert!(style.dim);
        assert!(style.blink);
        assert!(style.strikethrough);

        let ops = RenderOpBatchBuilder::new()
            .clear_rect(ExtensionRect::new(0, 0, 10, 2), RenderStyle::new())
            .border(
                ExtensionRect::new(0, 0, 10, 2),
                BorderGlyphs::rounded(),
                style,
            )
            .styled_text(
                1,
                1,
                vec![RenderTextSpan::new("ok", RenderStyle::new().bold())],
            )
            .cell_grid(
                2,
                2,
                vec![vec![
                    RenderCell::new('x', RenderStyle::new()),
                    RenderCell::sparse(RenderStyle::new()),
                ]],
            )
            .build();

        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[0], RenderOp::ClearRect { .. }));
        assert!(
            matches!(ops[1], RenderOp::Border { glyphs, .. } if glyphs == BorderGlyphs::rounded())
        );
        assert!(matches!(ops[2], RenderOp::StyledText { .. }));
        assert!(matches!(ops[3], RenderOp::CellGrid { .. }));
    }

    #[test]
    fn render_layer_scene_builder_creates_keyed_retained_items() {
        let scene = RenderLayerScene::builder()
            .revision(7)
            .text("header", 2, 1, 0, "HEAD", RenderStyle::new().bold())
            .fill_rect(
                "background",
                0,
                ExtensionRect::new(0, 0, 10, 1),
                ' ',
                RenderStyle::new(),
            )
            .border(
                "border",
                1,
                ExtensionRect::new(0, 0, 10, 3),
                BorderGlyphs::rounded(),
                RenderStyle::new(),
            )
            .cell_grid(
                "grid",
                3,
                0,
                1,
                vec![vec![RenderCell::new('x', RenderStyle::new())]],
            )
            .build();

        assert_eq!(scene.revision, Some(7));
        assert_eq!(scene.items.len(), 4);
        assert_eq!(scene.items[0].key, RenderSceneItemKey::new("header"));
        assert!(matches!(
            scene.items[0].item,
            RenderLayerItem::Op(RenderOp::TextRun { .. })
        ));
        assert!(matches!(
            scene.items[1].item,
            RenderLayerItem::Op(RenderOp::FillRect { .. })
        ));
        assert!(matches!(
            scene.items[2].item,
            RenderLayerItem::Op(RenderOp::Border { .. })
        ));
        assert!(matches!(
            scene.items[3].item,
            RenderLayerItem::Op(RenderOp::CellGrid { .. })
        ));
    }

    #[test]
    fn render_extension_retained_scene_default_uses_fallback_path() {
        let ext = RecordingExtension {
            name: "x".to_string(),
            removed: Mutex::new(Vec::new()),
        };

        assert!(
            ext.render_layer_scene_with_context(
                Uuid::nil(),
                &ExtensionRect::new(0, 0, 10, 3),
                RenderExtensionLayer::AfterPaneContent,
                &RenderExtensionContext::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn registry_tracks_registration_order() {
        let registry = RenderExtensionRegistry::new();
        let a = Arc::new(RecordingExtension {
            name: "a".to_string(),
            removed: Mutex::new(Vec::new()),
        }) as Arc<dyn AttachRenderExtension>;
        let b = Arc::new(RecordingExtension {
            name: "b".to_string(),
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

    /// An extension that overrides nothing paints nothing and promises no revision, so the host
    /// samples it every frame.
    #[test]
    fn extension_defaults_paint_nothing_and_report_no_revision() {
        let ext = RecordingExtension {
            name: "x".to_string(),
            removed: Mutex::new(Vec::new()),
        };
        let rect = ExtensionRect::new(0, 0, 1, 1);
        let context = RenderExtensionContext::default();
        for layer in [
            RenderExtensionLayer::BeforePaneContent,
            RenderExtensionLayer::AfterPaneContent,
        ] {
            assert!(
                ext.render_layer_scene_shared(Uuid::nil(), &rect, layer, &context)
                    .is_none()
            );
            assert_eq!(ext.render_layer_revision(Uuid::nil(), layer), None);
        }
    }

    #[test]
    fn extension_default_content_rect_override_is_none() {
        let ext = RecordingExtension {
            name: "x".to_string(),
            removed: Mutex::new(Vec::new()),
        };
        assert!(ext.content_rect_override(Uuid::nil()).is_none());
    }
}
