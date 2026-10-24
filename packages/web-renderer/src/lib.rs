//! Directional, domain-neutral browser output for the retained TUI compositor.
//!
//! No transport, runtime, plugin loader, DOM, or GPU type enters the contract.
//! Decoded frames are output data, never authoritative TUI state.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

mod local;
pub use local::LocalSurface;
mod contract;
pub mod input;
pub mod wire;
pub use wire::{
    BINARY_WIRE_VERSION, ImageResource, ResourceRequirement, decode_binary,
    decode_binary_with_resources, encode_binary, encode_binary_with_resources, encode_delta,
    encode_delta_with_resources, extract_resources, resource_requirements,
};
mod renderer;
pub use contract::{
    BrowserFrame, DecodeError, Limits, MetadataAvailability, MetadataKind, SurfaceProvenance,
    WIRE_VERSION, decode_json, encode_json,
};
pub use renderer::{BackendCapabilities, BrowserBackend, PresentError, Renderer, RequiredFeature};

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
mod canvas;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
pub use canvas::CanvasRenderer;
