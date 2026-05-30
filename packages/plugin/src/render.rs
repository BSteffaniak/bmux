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
//!    extension's [`AttachRenderExtension::render_surface`] for every
//!    damaged visible surface.
//! 3. When a surface disappears (pane closed, layout recomputed without
//!    it), the attach runtime calls
//!    [`AttachRenderExtension::surface_removed`] so the extension can
//!    evict any cached state.
//!
//! Extensions are expected to be lightweight: the registry lookup is
//! `O(n)` per render, and `render_surface` is on the hot path. Caching
//! paint output on the extension side is recommended when the source
//! data (e.g. a scene-protocol snapshot) changes less often than
//! layout refreshes.

use std::any::Any;
use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, OnceLock, RwLock};
use unicode_width::UnicodeWidthStr;
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    pub modifiers: AttachInputModifiers,
    pub focused_pane: Option<AttachInputPaneContext>,
    pub hovered_pane: Option<AttachInputPaneContext>,
}

#[allow(clippy::struct_excessive_bools)] // Independent routing flags returned by plugin input hooks.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachInputResult {
    pub consumed: bool,
    pub capture_pointer: bool,
    pub capture_keyboard: Vec<String>,
    pub release_capture: bool,
    pub dirty: bool,
    /// Optional user-facing status emitted by best-effort input hooks.
    ///
    /// This keeps input-hook health reporting generic: overloaded hooks can
    /// explain that they deferred work without failing the input path.
    #[serde(default)]
    pub status_message: Option<String>,
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
    u16::try_from(UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderColor {
    Default,
    Named(RenderNamedColor),
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderLayerItem {
    Op(RenderOp),
    Graphic(TerminalGraphicOverlay),
}

/// Retained declarative scene item for renderer-owned diffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderSceneItem {
    pub key: RenderSceneItemKey,
    pub z: i16,
    pub kind: RenderSceneItemKind,
}

impl RenderSceneItem {
    #[must_use]
    pub fn text(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        text: impl Into<String>,
        style: RenderStyle,
    ) -> Self {
        Self {
            key: RenderSceneItemKey::new(key),
            z,
            kind: RenderSceneItemKind::Text {
                x,
                y,
                text: text.into(),
                style,
            },
        }
    }

    #[must_use]
    pub fn styled_text(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        spans: impl Into<Vec<RenderTextSpan>>,
    ) -> Self {
        Self {
            key: RenderSceneItemKey::new(key),
            z,
            kind: RenderSceneItemKind::StyledText {
                x,
                y,
                spans: spans.into(),
            },
        }
    }

    #[must_use]
    pub const fn fill_rect(
        key: RenderSceneItemKey,
        z: i16,
        rect: ExtensionRect,
        ch: char,
        style: RenderStyle,
    ) -> Self {
        Self {
            key,
            z,
            kind: RenderSceneItemKind::FillRect { rect, ch, style },
        }
    }

    #[must_use]
    pub const fn border(
        key: RenderSceneItemKey,
        z: i16,
        rect: ExtensionRect,
        glyphs: BorderGlyphs,
        style: RenderStyle,
    ) -> Self {
        Self {
            key,
            z,
            kind: RenderSceneItemKind::Border {
                rect,
                glyphs,
                style,
            },
        }
    }

    #[must_use]
    pub fn cell_grid(
        key: impl Into<String>,
        z: i16,
        x: u16,
        y: u16,
        rows: impl Into<Vec<Vec<RenderCell>>>,
    ) -> Self {
        Self {
            key: RenderSceneItemKey::new(key),
            z,
            kind: RenderSceneItemKind::CellGrid {
                x,
                y,
                rows: rows.into(),
            },
        }
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
            kind: RenderSceneItemKind::TerminalGraphic { graphic },
        }
    }

    #[must_use]
    pub fn under_cells(
        key: impl Into<String>,
        z: i16,
        cells: impl Into<Vec<(u16, u16, RenderUnderCell)>>,
    ) -> Self {
        Self {
            key: RenderSceneItemKey::new(key),
            z,
            kind: RenderSceneItemKind::UnderCells {
                cells: cells.into(),
            },
        }
    }
}

/// Retained scene item payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderSceneItemKind {
    Text {
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
    TerminalGraphic {
        graphic: TerminalGraphicOverlay,
    },
    UnderCells {
        cells: Vec<(u16, u16, RenderUnderCell)>,
    },
}

/// Retained visual intent for one extension layer on one surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderLayerScene {
    pub revision: Option<u64>,
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

/// Context supplied to render extensions for one render pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderExtensionContext {
    pub capabilities: TerminalRenderCapabilities,
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

    /// Refresh any retained state that the extension consumes before a
    /// frame is rendered.
    ///
    /// The attach renderer calls this once per frame, before querying
    /// damage or render revisions. Implementations should make this a
    /// cheap non-blocking drain of already-retained state (for example,
    /// `watch::Receiver::borrow_and_update`), not an IPC call or other
    /// unbounded operation.
    fn refresh_state(&self) {}

    /// Return the currently-invalid region for a surface. The default
    /// is conservative: if the caller asks an extension to repaint and
    /// the extension cannot provide exact damage, the host treats the
    /// whole surface as damaged.
    fn surface_damage(&self, _surface_id: Uuid, _surface_rect: &ExtensionRect) -> RenderDamage {
        RenderDamage::FullSurface
    }

    /// Return the currently-invalid region for a surface on a specific
    /// layer. Implementations with layered output should override this;
    /// the default delegates to [`Self::surface_damage`] for backward
    /// compatibility.
    fn surface_layer_damage(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        _layer: RenderExtensionLayer,
    ) -> RenderDamage {
        self.surface_damage(surface_id, surface_rect)
    }

    /// Return whether this layer must be repainted after pane content is
    /// emitted for the same surface. Extensions that only draw outside the
    /// content rect can opt out so ordinary pane output does not promote their
    /// damage to a full-surface repaint. The default is conservative for
    /// overlay-style extensions and existing implementations.
    fn redraws_on_content_damage(&self, _layer: RenderExtensionLayer) -> bool {
        true
    }

    /// Return an optional revision token for render output on this
    /// surface. Declarative render ops are cached only when this
    /// returns `Some`; increment or otherwise change the token whenever
    /// the extension's output for the surface can change.
    fn render_revision(&self, _surface_id: Uuid) -> Option<u64> {
        None
    }

    /// Return an optional revision token for render output on this
    /// surface and layer. Implementations with layered output should
    /// override this; the default delegates to [`Self::render_revision`]
    /// for backward compatibility.
    fn render_layer_revision(&self, surface_id: Uuid, _layer: RenderExtensionLayer) -> Option<u64> {
        self.render_revision(surface_id)
    }

    /// Paint per-surface output onto `stdout` for the damaged region
    /// of `surface_rect`. Returns `Ok(true)` when any bytes were
    /// written, or `Ok(false)` when the extension had nothing to paint.
    ///
    /// # Errors
    ///
    /// Returns any error from queueing bytes onto `stdout`.
    fn render_surface(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
    ) -> io::Result<bool>;

    /// Paint per-surface output for one layer onto `stdout` for the
    /// damaged region of `surface_rect`. The default delegates after-content
    /// rendering to [`Self::render_surface`] and emits nothing for
    /// before-content rendering, preserving existing extension behavior.
    ///
    /// # Errors
    ///
    /// Returns any error from queueing bytes onto `stdout`.
    fn render_layer_surface(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
    ) -> io::Result<bool> {
        match layer {
            RenderExtensionLayer::BeforePaneContent => Ok(false),
            RenderExtensionLayer::AfterPaneContent => {
                self.render_surface(stdout, surface_id, surface_rect, damage)
            }
        }
    }

    /// Context-aware counterpart to [`Self::render_surface`]. The default
    /// preserves existing extension behavior.
    ///
    /// # Errors
    ///
    /// Returns any error from queueing bytes onto `stdout`.
    fn render_surface_with_context(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        _context: &RenderExtensionContext,
    ) -> io::Result<bool> {
        self.render_surface(stdout, surface_id, surface_rect, damage)
    }

    /// Context-aware counterpart to [`Self::render_layer_surface`].
    ///
    /// # Errors
    ///
    /// Returns any error from queueing bytes onto `stdout`.
    fn render_layer_surface_with_context(
        &self,
        stdout: &mut dyn io::Write,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> io::Result<bool> {
        match layer {
            RenderExtensionLayer::BeforePaneContent => Ok(false),
            RenderExtensionLayer::AfterPaneContent => {
                self.render_surface_with_context(stdout, surface_id, surface_rect, damage, context)
            }
        }
    }

    /// Return declarative render operations for the damaged region of
    /// `surface_rect`. Returning `None` asks the host to call
    /// [`Self::render_surface`] as an imperative escape hatch. Returning
    /// `Some(Vec::new())` means the extension is declarative but has no
    /// operations for this damage.
    fn render_ops(
        &self,
        _surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        _damage: &RenderDamage,
    ) -> Option<Vec<RenderOp>> {
        None
    }

    /// Return retained declarative visual intent for one layer.
    ///
    /// Implementations that return a scene let the host renderer own diffing,
    /// stale cleanup, content replay, z-ordering, terminal graphics lifecycle,
    /// and byte emission. The default returns `None` so existing damage-oriented
    /// extension APIs remain the compatibility fallback.
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

    /// Return z-ordered declarative render items for one layer and damaged
    /// region of `surface_rect`. Returning `None` asks the host to fall back to
    /// [`Self::render_layer_ops_with_context`] and then the imperative escape
    /// hatch. The default preserves existing operation-only extensions.
    fn render_layer_items_with_context(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<Vec<RenderLayerItem>> {
        let _ = (surface_id, surface_rect, damage, layer, context);
        None
    }

    /// Return declarative render operations for one layer and damaged
    /// region of `surface_rect`. Returning `None` asks the host to call
    /// [`Self::render_layer_surface`] as an imperative escape hatch.
    /// The default delegates after-content rendering to [`Self::render_ops`]
    /// and emits no before-content operations.
    fn render_layer_ops(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
    ) -> Option<Vec<RenderOp>> {
        match layer {
            RenderExtensionLayer::BeforePaneContent => Some(Vec::new()),
            RenderExtensionLayer::AfterPaneContent => {
                self.render_ops(surface_id, surface_rect, damage)
            }
        }
    }

    /// Context-aware counterpart to [`Self::render_ops`]. The default
    /// preserves existing extension behavior.
    fn render_ops_with_context(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        _context: &RenderExtensionContext,
    ) -> Option<Vec<RenderOp>> {
        self.render_ops(surface_id, surface_rect, damage)
    }

    /// Context-aware counterpart to [`Self::render_layer_ops`].
    fn render_layer_ops_with_context(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        layer: RenderExtensionLayer,
        context: &RenderExtensionContext,
    ) -> Option<Vec<RenderOp>> {
        match layer {
            RenderExtensionLayer::BeforePaneContent => Some(Vec::new()),
            RenderExtensionLayer::AfterPaneContent => {
                self.render_ops_with_context(surface_id, surface_rect, damage, context)
            }
        }
    }

    /// Return before-content cells for the damaged region of `surface_rect`.
    /// This is the composited counterpart to [`Self::render_layer_ops`] for
    /// [`RenderExtensionLayer::BeforePaneContent`]: cells returned here are
    /// merged into pane row rendering so pane content can repaint over them.
    fn render_before_content_cells(
        &self,
        _surface_id: Uuid,
        _surface_rect: &ExtensionRect,
        _damage: &RenderDamage,
    ) -> Option<Vec<(u16, u16, RenderUnderCell)>> {
        Some(Vec::new())
    }

    /// Context-aware counterpart to [`Self::render_before_content_cells`].
    fn render_before_content_cells_with_context(
        &self,
        surface_id: Uuid,
        surface_rect: &ExtensionRect,
        damage: &RenderDamage,
        _context: &RenderExtensionContext,
    ) -> Option<Vec<(u16, u16, RenderUnderCell)>> {
        self.render_before_content_cells(surface_id, surface_rect, damage)
    }

    /// Override the surface's content-rect inset. Returning `Some`
    /// tells the attach runtime "the PTY should render inside this
    /// smaller rectangle"; `None` means the extension has no opinion.
    /// When multiple extensions return `Some`, the attach runtime
    /// picks the narrowest inset.
    fn content_rect_override(&self, _surface_id: Uuid) -> Option<ExtensionRect> {
        None
    }

    /// Return coarse input hooks currently owned by this extension.
    /// Hooks are event subscriptions, not hit-test regions: the host
    /// applies the cheap filter and the owning plugin/script decides
    /// whether a specific event is consumed.
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

#[must_use]
pub fn global_visual_adapter_registry() -> Arc<VisualAdapterRegistry> {
    static GLOBAL: OnceLock<Arc<VisualAdapterRegistry>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(VisualAdapterRegistry::new()))
        .clone()
}

pub fn register_visual_adapter(adapter: Arc<dyn AttachVisualAdapter>) {
    global_visual_adapter_registry().register(adapter);
}

#[must_use]
pub fn registered_visual_adapter(id: &str) -> Option<Arc<dyn AttachVisualAdapter>> {
    global_visual_adapter_registry().get(id)
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

        fn render_surface(
            &self,
            _stdout: &mut dyn io::Write,
            surface_id: Uuid,
            _surface_rect: &ExtensionRect,
            _damage: &RenderDamage,
        ) -> io::Result<bool> {
            self.applied.lock().unwrap().push(surface_id);
            Ok(false)
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
            scene.items[0].kind,
            RenderSceneItemKind::Text { .. }
        ));
        assert!(matches!(
            scene.items[1].kind,
            RenderSceneItemKind::FillRect { .. }
        ));
        assert!(matches!(
            scene.items[2].kind,
            RenderSceneItemKind::Border { .. }
        ));
        assert!(matches!(
            scene.items[3].kind,
            RenderSceneItemKind::CellGrid { .. }
        ));
    }

    #[test]
    fn render_extension_retained_scene_default_uses_fallback_path() {
        let ext = RecordingExtension {
            name: "x".to_string(),
            applied: Mutex::new(Vec::new()),
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
    fn extension_default_damage_is_full_surface() {
        let ext = RecordingExtension {
            name: "x".to_string(),
            applied: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
        };
        assert_eq!(
            ext.surface_damage(
                Uuid::nil(),
                &ExtensionRect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
            ),
            RenderDamage::FullSurface
        );
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
