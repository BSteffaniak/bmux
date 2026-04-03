//! Terminal image protocol support for bmux.
//!
//! This crate is the containment boundary for all image-related logic:
//! sequence interception, per-pane registries, protocol codecs, host
//! capability detection, and the compositor overlay layer.
//!
//! Feature-gated per protocol: `sixel`, `kitty`, `iterm2`.

pub mod codec;
pub mod compositor;
pub mod config;
pub mod host_caps;
pub mod intercept;
pub mod ipc_convert;
pub mod model;
pub mod registry;

pub use config::ImageConfig;
pub use host_caps::HostImageCapabilities;
pub use intercept::{ImageInterceptor, InterceptResult};
pub use model::*;
pub use registry::ImageRegistry;
