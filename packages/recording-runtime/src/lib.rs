//! Neutral primitive crate for the recording-plugin domain.
//!
//! Hosts the `RecordingSink` trait, a handle newtype for registry
//! lookup, `RecordMeta` (per-event metadata), and a `DefaultNoOp`
//! fallback impl. `RecordingRuntime` concrete + file I/O live in the
//! plugin impl crate.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_ipc::{RecordingEventKind, RecordingPayload};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// Per-event metadata attached to each record written through the
/// sink. Lives here (not in `bmux_ipc`) because it is shared between
/// server's write path and the recording plugin's runtime; neither
/// side needs to wire-serialize it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordMeta {
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub pane_id: Option<Uuid>,
    #[serde(default)]
    pub client_id: Option<Uuid>,
}

/// Fast-path recording write contract, implemented by the recording
/// plugin and stored in the plugin state registry so server can look
/// it up and write to it without depending on the plugin impl crate.
///
/// The trait is intentionally narrow — a single `record` method — so
/// the implementation can mutex-guard internal runtimes without
/// forcing the contract to leak runtime handles.
pub trait RecordingSink: Send + Sync {
    /// Write a single record into whatever runtimes are active. The
    /// call is expected to be cheap (lock a mutex, append to a
    /// channel) and must not block on disk I/O.
    fn record(&self, kind: RecordingEventKind, payload: RecordingPayload, meta: RecordMeta);
}

/// Registry newtype wrapping an `Arc<dyn RecordingSink>`. Server reads
/// this on every pane-output event and calls `.record()`.
#[derive(Clone)]
pub struct RecordingSinkHandle(pub Arc<dyn RecordingSink>);

impl RecordingSinkHandle {
    #[must_use]
    pub fn new<S: RecordingSink + 'static>(sink: S) -> Self {
        Self(Arc::new(sink))
    }

    #[must_use]
    pub fn from_arc(sink: Arc<dyn RecordingSink>) -> Self {
        Self(sink)
    }

    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopRecordingSink)
    }
}

/// No-op default impl. Registered by server at startup; the recording
/// plugin overwrites during `activate`.
#[derive(Debug, Default)]
pub struct NoopRecordingSink;

impl RecordingSink for NoopRecordingSink {
    fn record(&self, _kind: RecordingEventKind, _payload: RecordingPayload, _meta: RecordMeta) {}
}
