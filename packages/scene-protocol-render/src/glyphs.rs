//! Re-export of the border-glyph lookup helpers from
//! [`bmux_scene_protocol`].
//!
//! These are accessed through this crate (rather than imported
//! directly from `bmux_scene_protocol::glyphs`) so render-extension
//! callers can depend on a single rendering-vocabulary crate rather
//! than pulling the wire-schema crate in directly. The re-export is
//! transparent — the types are identical.

pub use bmux_scene_protocol::glyphs::{
    BorderGlyphSet, border_glyphs_corners, border_glyphs_corners_or_custom,
};
