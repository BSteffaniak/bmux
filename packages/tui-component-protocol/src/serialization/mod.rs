//! Feature-gated serialization helpers.

#[cfg(feature = "bmux-codec")]
pub mod codec;
#[cfg(feature = "serde-json")]
pub mod json;
