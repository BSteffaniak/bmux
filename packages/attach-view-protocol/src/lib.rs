#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

//! Neutral attach view change protocol DTOs.

/// Coarse components of an attached view that may need resynchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachViewComponent {
    Scene,
    SurfaceContent,
    Layout,
}
