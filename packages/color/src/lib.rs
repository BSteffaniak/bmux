#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Terminal-independent color values and perceptual color math.
//!
//! This crate owns color *values* only. It has no terminal, frame, or style
//! types, so renderer-neutral layers may depend on it without acquiring
//! terminal concepts.
//!
//! * [`Rgb`] is an opaque 8-bit sRGB color with hex parsing and WCAG
//!   luminance/contrast helpers.
//! * [`Oklab`] and [`Oklch`] are perceptual color spaces used for mixing and
//!   lightness/chroma adjustment.
//! * [`mix`], [`lighten`], [`darken`], [`saturate`], [`desaturate`], and
//!   [`ensure_contrast`] derive colors from other colors.
//! * [`Gradient`] samples perceptually uniform gradients.
//! * [`ansi`] maps colors onto the standard 256- and 16-color palettes.

pub mod ansi;
mod gradient;
mod oklab;
mod ops;
mod rgb;

pub use gradient::{Gradient, GradientError};
pub use oklab::{Oklab, Oklch};
pub use ops::{darken, desaturate, ensure_contrast, lighten, mix, saturate};
pub use rgb::{ParseHexError, Rgb};

/// Largest possible WCAG contrast ratio (white on black).
pub const MAX_CONTRAST_RATIO: f64 = 21.0;
