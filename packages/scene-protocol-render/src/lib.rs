//! Shared helpers for renderers of [`bmux_scene_protocol`] paint commands: terminal capability
//! matching and the style interpolation gradients use.
//!
//! Paint commands are the decoration authoring vocabulary (the `bmux.scene` contract, which
//! themes and scripts write). A renderer lowers them once into the host's render items
//! (`bmux_plugin::RenderSceneItem`); no decoration-plugin-specific knowledge lives here.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod capabilities;
pub mod paint;

pub use capabilities::{SceneRenderCapabilities, capability_query_matches};
pub use paint::{interpolate_style, opaque_row_text};
