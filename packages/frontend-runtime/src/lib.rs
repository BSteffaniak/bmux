//! Shared attachment lifecycle and retained presentation contracts.
//!
//! This crate never opens a window, terminal, web listener, or runtime root.
//! Providers own execution and domain policy; hosts own successful presentation.

pub mod driver;
pub mod input;
pub mod presentation;
pub mod state;

pub use driver::{DriverEvent, NativeAttachDriver};
pub use state::AttachState;
