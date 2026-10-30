//! Bterm's process boundary: argument selection, native window lifecycle, and
//! wiring to BMUX's existing attach frontend. This crate owns no terminal,
//! plugin resources, layout, or hit-test state. Closing its window detaches the
//! frontend; the explicitly selected companion remains independent.

pub mod app;
pub mod compatibility;
pub mod configuration;
#[cfg(target_os = "linux")]
mod desktop;
pub mod headless;
mod managed_storage;
pub mod pane_zoom;
mod reconciliation;
pub mod runtime_launch;
pub mod runtime_paths;
pub mod runtime_selection;
pub mod typography;
pub mod window;
mod window_typography;
pub mod zoom;
pub mod zoom_preferences;
