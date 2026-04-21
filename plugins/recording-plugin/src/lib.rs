//! bmux recording plugin — owns `RecordingRuntime` handles and
//! recording lifecycle operations.
//!
//! Two responsibilities:
//!
//! - Fast-path writes: implements [`RecordingSink`] and registers the
//!   handle into the plugin state registry. Server reads the handle on
//!   every pane-output event and writes via the sink. This keeps the
//!   hot path allocation-free and free of service-dispatch overhead,
//!   without forcing `packages/server` to depend on this crate.
//!
//! - Control plane: typed service dispatch for the recording
//!   operations (start, stop, status, list, delete, cut, rolling-*,
//!   prune, write-custom-event, capture-targets, delete-all). The
//!   service interface and wire types live in
//!   `bmux_recording_plugin_api`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_recording_plugin_api::{RecordMeta, RecordingRuntime, RecordingSink};
use std::sync::{Arc, Mutex};

/// `RecordingSink` impl that fans out each record to both the manual
/// and rolling `RecordingRuntime` handles.
pub struct DualRuntimeSink {
    manual: Arc<Mutex<RecordingRuntime>>,
    rolling: Arc<Mutex<Option<RecordingRuntime>>>,
}

impl DualRuntimeSink {
    #[must_use]
    pub const fn new(
        manual: Arc<Mutex<RecordingRuntime>>,
        rolling: Arc<Mutex<Option<RecordingRuntime>>>,
    ) -> Self {
        Self { manual, rolling }
    }
}

impl RecordingSink for DualRuntimeSink {
    fn record(
        &self,
        kind: bmux_ipc::RecordingEventKind,
        payload: bmux_ipc::RecordingPayload,
        meta: RecordMeta,
    ) {
        if let Ok(runtime) = self.manual.lock() {
            let _ = runtime.record(kind, payload.clone(), meta);
        }
        if let Ok(runtime) = self.rolling.lock()
            && let Some(runtime) = runtime.as_ref()
        {
            let _ = runtime.record(kind, payload, meta);
        }
    }
}
