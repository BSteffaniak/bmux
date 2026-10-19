//! Bterm's process boundary: argument selection, native window lifecycle, and
//! wiring to BMUX's existing attach frontend. This crate owns no terminal,
//! plugin resources, layout, or hit-test state. Closing its window detaches the
//! frontend; the explicitly selected companion remains independent.

pub mod app;
pub mod compatibility;
pub mod runtime_launch;
pub mod runtime_paths;
pub mod runtime_selection;
pub mod window;
