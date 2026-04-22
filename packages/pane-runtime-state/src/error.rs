//! `SessionRuntimeError` — shared by session-runtime call-sites.

use thiserror::Error;

/// Shared error type for session-runtime operations (lookup, write,
/// attach lifecycle). Carried by the plugin's typed service surface
/// and also used internally by the `PaneOutputReader` trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SessionRuntimeError {
    /// Session or pane not found in the runtime manager.
    #[error("session runtime not found")]
    NotFound,
    /// Client is not attached to the session.
    #[error("client is not attached to this session")]
    NotAttached,
    /// Pane or session is closed / shutting down.
    #[error("session runtime is closed")]
    Closed,
}
