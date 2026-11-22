//! Neutral paint vocabulary shared by every presentation producer.
//!
//! Rectangles, styles, ops, terminal graphics and keyed scene items, used by
//! producers and the attach host alike. `bmux_plugin::render` re-exports it.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum RenderExtensionLayer {
    /// Paint before pane terminal content.
    BeforePaneContent,
    /// Paint after pane terminal content.
    AfterPaneContent,
}

/// One cell from a before-pane-content render layer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderUnderCell {
    pub ch: char,
    pub style: RenderStyle,
}

/// Declarative paint operation emitted by an attach render extension.
///
/// Coordinates are absolute terminal display-cell coordinates in the same
/// coordinate space as the `surface_rect` passed to `AttachRenderExtension`.
/// The attach runtime clips every operation to that surface rectangle before
/// lowering it to terminal output. [`RenderDamage::Regions`] also uses this
/// absolute coordinate space.
///
/// Text bounds and clipping are measured in Unicode display cells, not UTF-8
/// bytes or scalar count. Implementations should prefer `TextRun` for strings
/// and use `CellGrid` only for already-cell-addressed one-column glyphs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
    /// Grapheme-aware cells with per-cell transparency, positioned by the
    /// block's own `x`/`y`. Transparent cells (`symbol: None`) keep what is
    /// beneath them; a wide symbol covers the transparent cells after it.
    Cells(crate::scene::CellBlock),
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

    #[must_use]
    pub const fn cells(block: crate::scene::CellBlock) -> Self {
        Self::Cells(block)
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RenderLayerItem {
    Op(RenderOp),
    Graphic(TerminalGraphicOverlay),
}

/// Retained declarative scene item for renderer-owned diffing: a keyed, z-ordered
/// [`RenderLayerItem`], the same vocabulary the host paints with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
            Self::Cells(block) => vec![block.rect()],
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenderLayerScene {
    /// Optional freshness token for diagnostics; the host's reuse is gated on
    /// `AttachRenderExtension::render_layer_revision`, not on this field.
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
