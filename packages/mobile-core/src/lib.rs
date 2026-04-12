#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared mobile domain primitives for bmux clients.

pub mod connection;
pub mod error;
pub mod ssh;
pub mod target;

pub use connection::{ConnectionManager, ConnectionRequest, ConnectionState, ConnectionStatus};
pub use error::{MobileCoreError, Result};
pub use ssh::observe_ssh_host_key_fingerprint_sha256;
pub use ssh::{EmbeddedSshBackend, MockSshBackend, SshBackend, SshTarget, parse_ssh_target};
pub use target::{
    CanonicalTarget, TargetInput, TargetRecord, TargetTransport, TargetUri, canonicalize_target,
};
