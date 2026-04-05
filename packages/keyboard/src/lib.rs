#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Key types and keyboard protocol encoding/decoding for bmux.
//!
//! This crate provides:
//! - Canonical key types ([`KeyCode`], [`Modifiers`], [`KeyStroke`])
//! - Kitty keyboard protocol (CSI u) encoding and decoding ([`csi_u`]) (behind the `csi-u` feature)
//! - Legacy VT/xterm encoding and decoding ([`legacy`])
//! - Unified encoding entry point ([`encode`])

#[cfg(feature = "csi-u")]
pub mod csi_u;
pub mod encode;
pub mod legacy;
pub mod types;

pub use types::{KeyCode, KeyStroke, Modifiers};
