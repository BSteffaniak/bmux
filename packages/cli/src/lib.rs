#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Command-line interface for bmux terminal multiplexer
//!
//! This package provides the command-line interface functionality for bmux.

pub use bmux_config::{BmuxConfig, ConfigPaths};
pub use bmux_event::{Event, EventDispatcher, ModalSystem, Mode};
pub use bmux_session::{SessionId, SessionInfo, SessionManager};
pub use bmux_terminal::{TerminalInstance, TerminalManager};

// CLI-specific functionality can be added here
// For now, this mainly serves as a re-export for CLI operations
