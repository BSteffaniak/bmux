//! Wgpu output for the retained BMUX presentation. No resource, layout, hit-test,
//! or interaction authority is owned here. Successful output is reported to the
//! caller; only the caller may commit the corresponding presentation transaction.

mod damage;
mod fonts;
mod gpu;
mod images;
mod renderer;
pub mod text;
mod text_cache;

pub use renderer::{
    FrameTelemetry, NativeRenderer, PresentReport, RenderError, RendererOptions, TelemetrySummary,
};
