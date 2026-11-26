#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared mobile domain primitives for bmux clients.
//!
//! A mobile client is a paired device: it pairs with a host through the
//! host's `bmux-pair:` link and reaches saved hosts through the connections
//! plugin's connector, exactly as `bmux connect` does on a desktop.

pub mod connection;
pub mod error;
pub mod paired_host;
mod pane_runtime_client;
pub mod pointer_output;
pub mod remote_bridge;

pub use connection::{
    ConnectionManager, ConnectionRequest, ConnectionState, ConnectionStatus, TerminalChunk,
    TerminalChunkKind, TerminalDiagnostic, TerminalMouseButton, TerminalMouseEvent,
    TerminalMouseEventKind, TerminalOpenRequest, TerminalSessionState, TerminalSessionStatus,
    TerminalSize, TerminalStatusSeverity,
};
pub use error::{MobileCoreError, Result};
pub use paired_host::{
    PairedHost, PairedHostRole, PairedHostStore, PairingRequest, PairingState, PairingStatus,
};
