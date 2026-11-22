#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Neutral retained presentation contract for bmux.
//!
//! [`paint`] is the drawing vocabulary every producer shares. The scene model
//! built on it is described in `docs/development/unified-presentation-plan.md`.

pub mod paint;
pub mod scene;

pub use paint::{
    BorderGlyphs, ExtensionRect, RenderCell, RenderColor, RenderDamage, RenderExtensionLayer,
    RenderLayerItem, RenderLayerScene, RenderLayerSceneBuilder, RenderNamedColor, RenderOp,
    RenderOpBatchBuilder, RenderSceneItem, RenderSceneItemKey, RenderStyle, RenderTextSpan,
    RenderUnderCell, TerminalGraphicFill, TerminalGraphicOverlay, TerminalRenderCapabilities,
    TerminalRgba, border_outline_rects, clip_render_text_run_to_rect,
    render_char_display_width_u16, render_single_display_cell_char, render_text_width_u16,
};
