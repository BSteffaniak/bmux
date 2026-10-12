//! Directional, domain-neutral browser output for the retained TUI compositor.
//!
//! No transport, runtime, plugin loader, DOM, or GPU type enters the contract.
//! Decoded frames are output data, never authoritative TUI state.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

mod contract;
mod renderer;
pub use contract::{
    BrowserFrame, DecodeError, Limits, SurfaceProvenance, WIRE_VERSION, decode_json, encode_json,
};
pub use renderer::{BrowserBackend, PresentError, Renderer};

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
mod canvas;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
pub use canvas::CanvasRenderer;
