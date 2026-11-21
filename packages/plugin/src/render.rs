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

/// Minimal rectangle used by render extensions to report content-rect
/// adjustments back to the attach runtime.
///
/// This is structurally identical to the scene-protocol `Rect` but is
/// defined here to keep the extension trait free of a
/// scene-protocol dependency: generic extensions (e.g. a future
/// overlay plugin that doesn't produce scene-protocol output) can
/// still speak the trait without importing wire-schema types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ExtensionRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl ExtensionRect {
    #[must_use]
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub const fn right(self) -> u16 {
        self.x.saturating_add(self.w)
    }

    #[must_use]
    pub const fn bottom(self) -> u16 {
        self.y.saturating_add(self.h)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }

    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self {
            x,
            y,
            w: right.saturating_sub(x),
            h: bottom.saturating_sub(y),
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    pub modifiers: AttachInputModifiers,
    pub focused_pane: Option<AttachInputPaneContext>,
    pub hovered_pane: Option<AttachInputPaneContext>,
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

/// Return the Unicode display-cell width of `text`, saturated to `u16::MAX`.
///
/// Declarative render APIs use display cells for text damage, clipping, and
/// cursor advance. This helper centralizes that contract so extensions and the
/// host do not duplicate subtly different width calculations.
#[must_use]
pub fn render_text_width_u16(text: &str) -> u16 {
    u16::try_from(bmux_cell_geometry::display_width(text)).unwrap_or(u16::MAX)
}

/// Return the Unicode display-cell width of a single scalar value.
#[must_use]
pub fn render_char_display_width_u16(ch: char) -> u16 {
    let mut buffer = [0; 4];
    render_text_width_u16(ch.encode_utf8(&mut buffer))
}

/// Return the single character in `value` only when it occupies exactly one
/// display cell. Empty, multi-scalar, zero-width, and wide strings return
/// `None` so callers can fall back conservatively.
#[must_use]
pub fn render_single_display_cell_char(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || render_char_display_width_u16(ch) != 1 {
        None
    } else {
        Some(ch)
    }
}

/// Clip a text run to `bounds` without splitting wide glyphs across display
/// cell boundaries.
///
/// `x` is the run's absolute display-cell column. The returned column is the
/// first emitted display cell. Zero-width characters are retained only after a
/// preceding emitted character has established a visible starting column, or
/// when their cursor position lies inside `bounds`.
#[must_use]
pub fn clip_render_text_run_to_rect(
    x: u16,
    text: &str,
    bounds: ExtensionRect,
) -> Option<(u16, String)> {
    let clip_left = bounds.x;
    let clip_right = bounds.right();
    let mut cursor = x;
    let mut clipped_x = None;
    let mut clipped = String::new();
    for ch in text.chars() {
        let width = render_char_display_width_u16(ch);
        let next = cursor.saturating_add(width);
        let include = if width == 0 {
            clipped_x.is_some() || (cursor >= clip_left && cursor < clip_right)
        } else {
            next > clip_left && cursor < clip_right && cursor >= clip_left && next <= clip_right
        };
        if include {
            clipped_x.get_or_insert(cursor);
            clipped.push(ch);
        } else if cursor >= clip_right {
            break;
        }
        cursor = next;
    }
    clipped_x.map(|x| (x, clipped))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderColor {
    Default,
    Named(RenderNamedColor),
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

impl RenderColor {
    /// Resolve a color to a terminal-supported representation.
    #[must_use]
    pub fn for_capabilities(self, capabilities: TerminalRenderCapabilities) -> Self {
        match self {
            Self::Rgb { r, g, b } if !capabilities.truecolor => {
                Self::Indexed(rgb_to_ansi256(r, g, b))
            }
            color => color,
        }
    }
}

fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    fn component(value: u8) -> u16 {
        (u16::from(value) * 5 + 127) / 255
    }

    let indexed = 16 + 36 * component(r) + 6 * component(g) + component(b);
    u8::try_from(indexed).expect("ANSI 256-color cube index is bounded")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderNamedColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

#[allow(clippy::struct_excessive_bools)] // Terminal SGR flags are independent style attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RenderStyle {
    pub fg: Option<RenderColor>,
    pub bg: Option<RenderColor>,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
    pub reverse: bool,
    pub dim: bool,
    pub blink: bool,
    pub strikethrough: bool,
}

impl RenderStyle {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fg: None,
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

    #[must_use]
    pub const fn foreground(mut self, color: RenderColor) -> Self {
        self.fg = Some(color);
        self
    }

    #[must_use]
    pub const fn background(mut self, color: RenderColor) -> Self {
        self.bg = Some(color);
        self
    }

    #[must_use]
    pub const fn named_foreground(self, color: RenderNamedColor) -> Self {
        self.foreground(RenderColor::Named(color))
    }

    #[must_use]
    pub const fn named_background(self, color: RenderNamedColor) -> Self {
        self.background(RenderColor::Named(color))
    }

    #[must_use]
    pub const fn rgb_foreground(self, r: u8, g: u8, b: u8) -> Self {
        self.foreground(RenderColor::Rgb { r, g, b })
    }

    #[must_use]
    pub const fn rgb_background(self, r: u8, g: u8, b: u8) -> Self {
        self.background(RenderColor::Rgb { r, g, b })
    }

    #[must_use]
    pub const fn indexed_foreground(self, color: u8) -> Self {
        self.foreground(RenderColor::Indexed(color))
    }

    #[must_use]
    pub const fn indexed_background(self, color: u8) -> Self {
        self.background(RenderColor::Indexed(color))
    }

    /// Resolve capability-dependent style values before terminal lowering.
    #[must_use]
    pub fn for_capabilities(mut self, capabilities: TerminalRenderCapabilities) -> Self {
        self.fg = self.fg.map(|color| color.for_capabilities(capabilities));
        self.bg = self.bg.map(|color| color.for_capabilities(capabilities));
        self
    }

    #[must_use]
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    #[must_use]
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    #[must_use]
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    #[must_use]
    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    #[must_use]
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    #[must_use]
    pub const fn blink(mut self) -> Self {
        self.blink = true;
        self
    }

    #[must_use]
    pub const fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderTextSpan {
    pub text: String,
    pub style: RenderStyle,
}

impl RenderTextSpan {
    #[must_use]
    pub fn new(text: impl Into<String>, style: RenderStyle) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenderCell {
    /// Display-cell glyph to paint. `None` represents a transparent/sparse
    /// cell: it reserves a grid position but emits no terminal bytes.
    pub ch: Option<char>,
    pub style: RenderStyle,
}

impl RenderCell {
    #[must_use]
    pub const fn new(ch: char, style: RenderStyle) -> Self {
        Self {
            ch: Some(ch),
            style,
        }
    }

    #[must_use]
    pub const fn sparse(style: RenderStyle) -> Self {
        Self { ch: None, style }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BorderGlyphs {
    pub top_left: char,
    pub top_right: char,
    pub bottom_left: char,
    pub bottom_right: char,
    pub horizontal: char,
    pub vertical: char,
}

impl BorderGlyphs {
    #[must_use]
    pub const fn ascii() -> Self {
        Self {
            top_left: '+',
            top_right: '+',
            bottom_left: '+',
            bottom_right: '+',
            horizontal: '-',
            vertical: '|',
        }
    }

    #[must_use]
    pub const fn rounded() -> Self {
        Self {
            top_left: '╭',
            top_right: '╮',
            bottom_left: '╰',
            bottom_right: '╯',
            horizontal: '─',
            vertical: '│',
        }
    }

    #[must_use]
    pub const fn square() -> Self {
        Self {
            top_left: '┌',
            top_right: '┐',
            bottom_left: '└',
            bottom_right: '┘',
            horizontal: '─',
            vertical: '│',
        }
    }

    /// Use ASCII borders when the terminal cannot reliably draw Unicode boxes.
    #[must_use]
    pub const fn for_capabilities(self, capabilities: TerminalRenderCapabilities) -> Self {
        if capabilities.unicode_box_drawing {
            self
        } else {
            Self::ascii()
        }
    }
}

impl Default for BorderGlyphs {
    fn default() -> Self {
        Self::ascii()
    }
}

/// Render-extension layer currently being painted relative to pane
/// terminal content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RenderExtensionLayer {
    /// Paint before pane terminal content.
    BeforePaneContent,
    /// Paint after pane terminal content.
    AfterPaneContent,
}

/// One cell from a before-pane-content render layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderUnderCell {
    pub ch: char,
    pub style: RenderStyle,
}

/// Declarative paint operation emitted by an attach render extension.
///
/// Coordinates are absolute terminal display-cell coordinates in the same
/// coordinate space as the `surface_rect` passed to [`AttachRenderExtension`].
/// The attach runtime clips every operation to that surface rectangle before
/// lowering it to terminal output. [`RenderDamage::Regions`] also uses this
/// absolute coordinate space.
///
/// Text bounds and clipping are measured in Unicode display cells, not UTF-8
/// bytes or scalar count. Implementations should prefer `TextRun` for strings
/// and use `CellGrid` only for already-cell-addressed one-column glyphs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RenderOp {
    TextRun {
        x: u16,
        y: u16,
        text: String,
        style: RenderStyle,
    },
    StyledText {
        x: u16,
        y: u16,
        spans: Vec<RenderTextSpan>,
    },
    /// Clear a rectangle by painting spaces with `style`. This is semantically
    /// distinct from `FillRect { ch: ' ', .. }` so lower layers can eventually
    /// choose terminal erase primitives when the style is compatible.
    ClearRect {
        rect: ExtensionRect,
        style: RenderStyle,
    },
    /// Clear one row segment by painting `width` spaces with `style`.
    /// Semantically equivalent to a one-row `ClearRect`, but useful for
    /// adapters that naturally produce row-oriented erases.
    EraseRowSegment {
        x: u16,
        y: u16,
        width: u16,
        style: RenderStyle,
    },
    FillRect {
        rect: ExtensionRect,
        ch: char,
        style: RenderStyle,
    },
    Border {
        rect: ExtensionRect,
        glyphs: BorderGlyphs,
        style: RenderStyle,
    },
    CellGrid {
        x: u16,
        y: u16,
        rows: Vec<Vec<RenderCell>>,
    },
}

impl RenderOp {
    #[must_use]
    pub fn text_run(x: u16, y: u16, text: impl Into<String>, style: RenderStyle) -> Self {
        Self::TextRun {
            x,
            y,
            text: text.into(),
            style,
        }
    }

    #[must_use]
    pub fn styled_text(x: u16, y: u16, spans: impl Into<Vec<RenderTextSpan>>) -> Self {
        Self::StyledText {
            x,
            y,
            spans: spans.into(),
        }
    }

    #[must_use]
    pub const fn clear_rect(rect: ExtensionRect, style: RenderStyle) -> Self {
        Self::ClearRect { rect, style }
    }

    #[must_use]
    pub const fn erase_row_segment(x: u16, y: u16, width: u16, style: RenderStyle) -> Self {
        Self::EraseRowSegment { x, y, width, style }
    }

    #[must_use]
    pub const fn fill_rect(rect: ExtensionRect, ch: char, style: RenderStyle) -> Self {
        Self::FillRect { rect, ch, style }
    }

    #[must_use]
    pub const fn border(rect: ExtensionRect, glyphs: BorderGlyphs, style: RenderStyle) -> Self {
        Self::Border {
            rect,
            glyphs,
            style,
        }
    }

    #[must_use]
    pub fn cell_grid(x: u16, y: u16, rows: impl Into<Vec<Vec<RenderCell>>>) -> Self {
        Self::CellGrid {
            x,
            y,
            rows: rows.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderOpBatchBuilder {
    ops: Vec<RenderOp>,
}

impl RenderOpBatchBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self { ops: Vec::new() }
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ops: Vec::with_capacity(capacity),
        }
    }

    #[must_use]
    pub fn text_run(mut self, x: u16, y: u16, text: impl Into<String>, style: RenderStyle) -> Self {
        self.ops.push(RenderOp::text_run(x, y, text, style));
        self
    }

    #[must_use]
    pub fn styled_text(mut self, x: u16, y: u16, spans: impl Into<Vec<RenderTextSpan>>) -> Self {
        self.ops.push(RenderOp::styled_text(x, y, spans));
        self
    }

    #[must_use]
    pub fn clear_rect(mut self, rect: ExtensionRect, style: RenderStyle) -> Self {
        self.ops.push(RenderOp::clear_rect(rect, style));
        self
    }

    #[must_use]
    pub fn erase_row_segment(mut self, x: u16, y: u16, width: u16, style: RenderStyle) -> Self {
        self.ops
            .push(RenderOp::erase_row_segment(x, y, width, style));
        self
    }

    #[must_use]
    pub fn fill_rect(mut self, rect: ExtensionRect, ch: char, style: RenderStyle) -> Self {
        self.ops.push(RenderOp::fill_rect(rect, ch, style));
        self
    }

    #[must_use]
    pub fn border(mut self, rect: ExtensionRect, glyphs: BorderGlyphs, style: RenderStyle) -> Self {
        self.ops.push(RenderOp::border(rect, glyphs, style));
        self
    }

    #[must_use]
    pub fn cell_grid(mut self, x: u16, y: u16, rows: impl Into<Vec<Vec<RenderCell>>>) -> Self {
        self.ops.push(RenderOp::cell_grid(x, y, rows));
        self
    }

    #[must_use]
    pub fn push(mut self, op: RenderOp) -> Self {
        self.ops.push(op);
        self
    }

    #[must_use]
    pub fn build(self) -> Vec<RenderOp> {
        self.ops
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderDamage {
    None,
    FullSurface,
    Regions(Vec<ExtensionRect>),
}

impl RenderDamage {
    #[must_use]
    pub const fn full_surface() -> Self {
        Self::FullSurface
    }

    #[must_use]
    pub fn from_rects(rects: impl IntoIterator<Item = ExtensionRect>) -> Self {
        let mut merged: Vec<ExtensionRect> = Vec::new();
        for rect in rects {
            if rect.is_empty() {
                continue;
            }
            if let Some(existing) = merged.iter_mut().find(|existing| existing.intersects(rect)) {
                *existing = existing.union(rect);
            } else {
                merged.push(rect);
            }
        }
        if merged.is_empty() {
            Self::None
        } else {
            Self::Regions(merged)
        }
    }

    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Rendering capabilities detected for the attached terminal.
///
/// These fields are intentionally generic host facts, not decoration-specific
/// policy. Render extensions can use them to choose deterministic fallbacks
/// without probing the terminal themselves.
#[allow(clippy::struct_excessive_bools)] // Independent terminal feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalRenderCapabilities {
    pub truecolor: bool,
    pub unicode_box_drawing: bool,
    pub unicode_block_elements: bool,
    pub kitty_graphics: bool,
    pub sixel: bool,
    pub iterm2_inline_images: bool,
    pub graphics_alpha: bool,
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
    /// The terminal implements left/right margins (DEC private mode 69 with DECSLRM), so a
    /// rectangle narrower than the screen can be scrolled in place.
    #[serde(default)]
    pub left_right_margins: bool,
}

impl Default for TerminalRenderCapabilities {
    fn default() -> Self {
        Self {
            truecolor: true,
            unicode_box_drawing: true,
            unicode_block_elements: true,
            kitty_graphics: false,
            sixel: false,
            iterm2_inline_images: false,
            graphics_alpha: false,
            cell_pixel_width: 0,
            cell_pixel_height: 0,
            left_right_margins: false,
        }
    }
}

impl TerminalRenderCapabilities {
    /// Compact stable-ish key suitable for extension render cache partitioning.
    #[must_use]
    pub fn cache_key(self) -> u64 {
        let mut key = 0_u64;
        key |= u64::from(self.truecolor);
        key |= u64::from(self.unicode_box_drawing) << 1;
        key |= u64::from(self.unicode_block_elements) << 2;
        key |= u64::from(self.kitty_graphics) << 3;
        key |= u64::from(self.sixel) << 4;
        key |= u64::from(self.iterm2_inline_images) << 5;
        key |= u64::from(self.graphics_alpha) << 6;
        key |= u64::from(self.cell_pixel_width) << 16;
        key |= u64::from(self.cell_pixel_height) << 32;
        key
    }

    #[must_use]
    pub const fn has_cell_pixels(self) -> bool {
        self.cell_pixel_width > 0 && self.cell_pixel_height > 0
    }
}

/// RGBA color used by terminal graphics primitives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalRgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// Pixel mask applied to a solid terminal graphics primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TerminalGraphicFill {
    Full,
    Top { thickness_px: u16 },
    Bottom { thickness_px: u16 },
    Left { thickness_px: u16 },
    Right { thickness_px: u16 },
}

/// Generic terminal graphics overlay primitive.
///
/// The host attach renderer owns protocol selection, image caching, placement,
/// and cleanup. Extensions provide stable semantic/pixel intent only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalGraphicOverlay {
    /// Stable extension-owned key for this graphic within a surface/layer.
    pub key: u64,
    /// Cell-aligned placement rectangle in absolute terminal coordinates.
    pub cell_rect: ExtensionRect,
    /// Pixel dimensions of the transmitted image. These normally correspond to
    /// `cell_rect` multiplied by the detected cell pixel dimensions.
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub color: TerminalRgba,
    pub fill: TerminalGraphicFill,
    /// Z-order hint for protocols that support it. Positive values render above
    /// terminal text in Kitty-compatible terminals.
    pub z_index: i16,
}

/// Stable renderer-diff key for one declarative retained scene item.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct RenderSceneItemKey(pub String);

impl RenderSceneItemKey {
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

/// A z-ordered item emitted by a render extension.
///
/// Text/cell items and terminal graphics share one ordering stream so
/// extensions do not need an imperative raw-protocol escape hatch to preserve
/// paint order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RenderLayerItem {
    Op(RenderOp),
    Graphic(TerminalGraphicOverlay),
}

/// Retained declarative scene item for renderer-owned diffing: a keyed, z-ordered
/// [`RenderLayerItem`], the same vocabulary the host paints with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSceneItem {
    pub key: RenderSceneItemKey,
    pub z: i16,
    pub item: RenderLayerItem,
}

impl RenderSceneItem {
    /// A keyed item painting `op`.
    #[must_use]
    pub const fn op(key: RenderSceneItemKey, z: i16, op: RenderOp) -> Self {
        Self {
            key,
            z,
            item: RenderLayerItem::Op(op),
        }
    }

    #[must_use]
    pub fn text(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        text: impl Into<String>,
        style: RenderStyle,
    ) -> Self {
        Self::op(
            RenderSceneItemKey::new(key),
            z,
            RenderOp::text_run(x, y, text, style),
        )
    }

    #[must_use]
    pub fn styled_text(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        spans: impl Into<Vec<RenderTextSpan>>,
    ) -> Self {
        Self::op(
            RenderSceneItemKey::new(key),
            z,
            RenderOp::styled_text(x, y, spans),
        )
    }

    #[must_use]
    pub const fn fill_rect(
        key: RenderSceneItemKey,
        z: i16,
        rect: ExtensionRect,
        ch: char,
        style: RenderStyle,
    ) -> Self {
        Self::op(key, z, RenderOp::fill_rect(rect, ch, style))
    }

    #[must_use]
    pub const fn border(
        key: RenderSceneItemKey,
        z: i16,
        rect: ExtensionRect,
        glyphs: BorderGlyphs,
        style: RenderStyle,
    ) -> Self {
        Self::op(key, z, RenderOp::border(rect, glyphs, style))
    }

    #[must_use]
    pub fn cell_grid(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        rows: impl Into<Vec<Vec<RenderCell>>>,
    ) -> Self {
        Self::op(
            RenderSceneItemKey::new(key),
            z,
            RenderOp::cell_grid(x, y, rows),
        )
    }

    #[must_use]
    pub const fn terminal_graphic(
        key: RenderSceneItemKey,
        z: i16,
        graphic: TerminalGraphicOverlay,
    ) -> Self {
        Self {
            key,
            z,
            item: RenderLayerItem::Graphic(graphic),
        }
    }
}

/// The cells a border around `rect` paints: its outline, not its inside.
#[must_use]
pub fn border_outline_rects(rect: ExtensionRect) -> Vec<ExtensionRect> {
    if rect.w <= 2 || rect.h <= 2 {
        return vec![rect];
    }
    let bottom = rect.y.saturating_add(rect.h - 1);
    let right = rect.x.saturating_add(rect.w - 1);
    let inner = rect.h - 2;
    vec![
        ExtensionRect::new(rect.x, rect.y, rect.w, 1),
        ExtensionRect::new(rect.x, bottom, rect.w, 1),
        ExtensionRect::new(rect.x, rect.y + 1, 1, inner),
        ExtensionRect::new(right, rect.y + 1, 1, inner),
    ]
}

fn text_rect(x: u16, y: u16, width: u16) -> ExtensionRect {
    ExtensionRect::new(x, y, width, u16::from(width > 0))
}

fn spans_width(spans: &[RenderTextSpan]) -> u16 {
    spans
        .iter()
        .map(|span| render_text_width_u16(&span.text))
        .fold(0, u16::saturating_add)
}

fn grid_rect<T>(x: u16, y: u16, rows: &[Vec<T>]) -> ExtensionRect {
    let width = rows
        .iter()
        .map(|row| u16::try_from(row.len()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    ExtensionRect::new(x, y, width, u16::try_from(rows.len()).unwrap_or(u16::MAX))
}

impl RenderOp {
    /// The cells this operation paints (a border paints only its outline).
    #[must_use]
    pub fn painted_rects(&self) -> Vec<ExtensionRect> {
        match self {
            Self::TextRun { x, y, text, .. } => {
                vec![text_rect(*x, *y, render_text_width_u16(text))]
            }
            Self::StyledText { x, y, spans } => vec![text_rect(*x, *y, spans_width(spans))],
            Self::ClearRect { rect, .. } | Self::FillRect { rect, .. } => vec![*rect],
            Self::Border { rect, .. } => border_outline_rects(*rect),
            Self::EraseRowSegment { x, y, width, .. } => {
                vec![ExtensionRect::new(*x, *y, *width, 1)]
            }
            Self::CellGrid { x, y, rows } => vec![grid_rect(*x, *y, rows)],
        }
    }
}

impl RenderLayerItem {
    /// The cells this item paints.
    #[must_use]
    pub fn painted_rects(&self) -> Vec<ExtensionRect> {
        match self {
            Self::Op(op) => op.painted_rects(),
            Self::Graphic(graphic) => vec![graphic.cell_rect],
        }
    }
}

impl RenderSceneItem {
    /// The cells this item paints (a border paints only its outline).
    #[must_use]
    pub fn painted_rects(&self) -> Vec<ExtensionRect> {
        self.item.painted_rects()
    }
}

/// Retained visual intent for one extension layer on one surface.
///
/// A scene should contain the full current set of items for the queried layer,
/// not just the items that changed. The renderer compares the scene with its
/// previously retained snapshot and owns cleanup, content replay, z-ordering,
/// terminal graphics reconciliation, and byte emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderLayerScene {
    /// Optional freshness token for diagnostics; the host's reuse is gated on
    /// [`AttachRenderExtension::render_layer_revision`], not on this field.
    ///
    /// Revision changes do not imply stale cleanup. Cleanup is derived from
    /// stable item keys plus item bounds/content fingerprints.
    pub revision: Option<u64>,
    /// Full current item set for this extension layer.
    pub items: Vec<RenderSceneItem>,
}

impl RenderLayerScene {
    #[must_use]
    pub const fn new(revision: Option<u64>, items: Vec<RenderSceneItem>) -> Self {
        Self { revision, items }
    }

    #[must_use]
    pub const fn builder() -> RenderLayerSceneBuilder {
        RenderLayerSceneBuilder::new()
    }
}

/// Builder for retained layer scenes.
///
/// Builder methods take stable string-like keys. Use keys that identify the
/// logical visual element across frames, not keys based on frame counters or
/// damage regions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RenderLayerSceneBuilder {
    revision: Option<u64>,
    items: Vec<RenderSceneItem>,
}

impl RenderLayerSceneBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            revision: None,
            items: Vec::new(),
        }
    }

    #[must_use]
    pub const fn revision(mut self, revision: u64) -> Self {
        self.revision = Some(revision);
        self
    }

    #[must_use]
    pub fn item(mut self, item: RenderSceneItem) -> Self {
        self.items.push(item);
        self
    }

    #[must_use]
    pub fn text(
        self,
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        text: impl Into<String>,
        style: RenderStyle,
    ) -> Self {
        self.item(RenderSceneItem::text(key, z, x, y, text, style))
    }

    #[must_use]
    pub fn fill_rect(
        self,
        key: impl Into<String>,
        z: i16,
        rect: ExtensionRect,
        ch: char,
        style: RenderStyle,
    ) -> Self {
        self.item(RenderSceneItem::fill_rect(
            RenderSceneItemKey::new(key),
            z,
            rect,
            ch,
            style,
        ))
    }

    #[must_use]
    pub fn border(
        self,
        key: impl Into<String>,
        z: i16,
        rect: ExtensionRect,
        glyphs: BorderGlyphs,
        style: RenderStyle,
    ) -> Self {
        self.item(RenderSceneItem::border(
            RenderSceneItemKey::new(key),
            z,
            rect,
            glyphs,
            style,
        ))
    }

    #[must_use]
    pub fn cell_grid(
        self,
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        rows: impl Into<Vec<Vec<RenderCell>>>,
    ) -> Self {
        self.item(RenderSceneItem::cell_grid(key, z, x, y, rows))
    }

    #[must_use]
    pub fn terminal_graphic(
        self,
        key: impl Into<String>,
        z: i16,
        graphic: TerminalGraphicOverlay,
    ) -> Self {
        self.item(RenderSceneItem::terminal_graphic(
            RenderSceneItemKey::new(key),
            z,
            graphic,
        ))
    }

    #[must_use]
    pub fn build(self) -> RenderLayerScene {
        RenderLayerScene::new(self.revision, self.items)
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
