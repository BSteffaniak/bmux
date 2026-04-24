//! Terminal-ANSI renderer for [`bmux_scene_protocol`] paint commands.
//!
//! The scene-protocol crate defines a wire schema for decoration
//! output; this crate turns that vocabulary into the corresponding
//! bytes on a [`std::io::Write`] target.
//!
//! Callers provide a `SurfaceDecoration` (or individual `PaintCommand`
//! values) plus a writable stream; the executor orders the commands
//! by `z`, emits the ANSI SGR prelude for each styled run, writes the
//! text, and appends a terminating reset so attributes don't leak
//! into subsequent surfaces.
//!
//! No decoration-plugin-specific knowledge lives here — it's a shared
//! helper consumed by both the core attach pipeline and any plugin's
//! render-extension crate.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod glyphs;
pub mod paint;
pub mod sgr;

pub use glyphs::{BorderGlyphSet, border_glyphs_corners, border_glyphs_corners_or_custom};
pub use paint::{apply_paint_command, apply_paint_commands, opaque_row_text};
pub use sgr::scene_style_sgr_prelude;
