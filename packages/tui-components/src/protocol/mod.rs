//! Feature-gated bindings between declarative protocol nodes and BMUX TUI components.
//!
//! The protocol crate remains pure data. This module provides opt-in adapters
//! that render protocol trees with concrete `bmux_tui_components` primitives and
//! allow hosts to register extension bindings.

mod bindings;
mod convert;
mod error;
mod render;

pub use bindings::{ProtocolBindings, ProtocolComponentBinding};
pub use convert::FromProtocolComponent;
pub use error::ProtocolComponentError;
pub use render::{ProtocolComponent, ProtocolTree};
