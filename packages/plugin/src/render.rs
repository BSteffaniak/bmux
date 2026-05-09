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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
