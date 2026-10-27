//! Native main-thread host. This crate owns windows, not BMUX resources or layout.
//! Rendering and interaction authority remain with the retained presentation consumer.

pub mod accessibility;
pub mod clipboard;
pub mod geometry;
pub mod ime;
pub mod input;
mod window;

pub use winit::window::CursorIcon;

pub use geometry::{LogicalSize, PhysicalRect, PhysicalSize, WindowMetrics};
pub use window::{
    NativeEventLoop, PlatformError, PlatformEvent, PlatformProxy, SurfaceTarget, WindowApplication,
    WindowContext, WindowOptions, run,
};
