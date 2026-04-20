#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Event system for bmux terminal multiplexer
//!
//! This package provides the event system including modal system
//! management and input event types.

// Re-export models for easy access
pub use bmux_event_models as models;

// Re-export commonly used types
pub use models::{Event, KeyCode, KeyEvent, KeyModifiers, Mode, MouseEvent, SystemEvent};
