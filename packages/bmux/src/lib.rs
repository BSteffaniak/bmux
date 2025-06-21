#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! bmux - Modern terminal multiplexer written in Rust
//!
//! bmux is a high-performance terminal multiplexer that introduces innovative
//! features like multi-client session sharing with independent views, vim-inspired
//! modal interactions, and extensible plugin architecture.
//!
//! This crate serves as an umbrella package that optionally re-exports all bmux
//! functionality based on enabled features.
//!
//! # Features
//!
//! ## Core Features
//! - `session` - Session management functionality
//! - `terminal` - Terminal handling and PTY management  
//! - `event` - Event system and modal interactions
//! - `config` - Configuration management
//!
//! ## Application Features
//! - `cli` - Command-line interface (enables the `bmux` binary)
//! - `server` - Server component for multi-client support
//! - `client` - Client component for connecting to sessions
//!
//! ## Extension Features
//! - `keybind` - Key binding system
//! - `history` - Scrollback history management
//! - `search` - Fuzzy search functionality
//! - `plugin` - Plugin system for extensibility
//!
//! ## Feature Groups
//! - `core` - Enables all core features
//! - `all` - Enables all features (default)
//!
//! # Usage
//!
//! ```rust
//! // Use specific functionality
//! #[cfg(feature = "session")]
//! use bmux::session;
//!
//! #[cfg(feature = "terminal")]
//! use bmux::terminal;
//! ```

// Core re-exports
#[cfg(feature = "session")]
pub use bmux_session as session;

#[cfg(feature = "terminal")]
pub use bmux_terminal as terminal;

#[cfg(feature = "event")]
pub use bmux_event as event;

#[cfg(feature = "config")]
pub use bmux_config as config;

// Application re-exports
#[cfg(feature = "cli")]
pub use bmux_cli as cli;

#[cfg(feature = "server")]
pub use bmux_server as server;

#[cfg(feature = "client")]
pub use bmux_client as client;

// Extension re-exports
#[cfg(feature = "keybind")]
pub use bmux_keybind as keybind;

#[cfg(feature = "history")]
pub use bmux_history as history;

#[cfg(feature = "search")]
pub use bmux_search as search;

#[cfg(feature = "plugin")]
pub use bmux_plugin as plugin;

/// Prelude module for commonly used types across all enabled features
pub mod prelude {
    #[cfg(feature = "session")]
    pub use crate::session::{Session, SessionId, SessionInfo, SessionManager};

    #[cfg(feature = "terminal")]
    pub use crate::terminal::{PaneSize, TerminalInstance, TerminalManager};

    #[cfg(feature = "event")]
    pub use crate::event::{Event, EventDispatcher, ModalSystem, Mode};

    #[cfg(feature = "config")]
    pub use crate::config::{BmuxConfig, ConfigPaths};
}
