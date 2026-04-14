#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared mobile domain primitives for bmux clients.

pub mod connection;
pub mod error;
pub mod remote_bridge;
pub mod ssh;
pub mod target;

pub use connection::{
    ConnectionManager, ConnectionRequest, ConnectionState, ConnectionStatus, TerminalChunk,
    TerminalChunkKind, TerminalDiagnostic, TerminalMouseButton, TerminalMouseEvent,
    TerminalMouseEventKind, TerminalOpenRequest, TerminalSessionState, TerminalSessionStatus,
    TerminalSize, TerminalStatusSeverity,
};
pub use error::{MobileCoreError, Result};
pub use ssh::{
    EmbeddedSshBackend, HostKeyPinSuggestion, MockSshBackend, ObservedHostKey, SshBackend,
    SshTarget, apply_pin_query_fragment_to_target, apply_pin_suggestion_to_target,
    observe_ssh_host_key, observe_ssh_host_key_fingerprint_sha256,
    observe_ssh_host_key_with_pin_suggestion, parse_ssh_target,
};
pub use target::{
    CanonicalTarget, TargetInput, TargetRecord, TargetTransport, TargetUri, canonicalize_target,
};
