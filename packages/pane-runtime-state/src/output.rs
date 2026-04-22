//! `PaneOutputReader` trait + handle newtype.
//!
//! This is the abstraction server's connection-scoped
//! `event_push_task` uses to read pane output on behalf of an
//! attached client, without naming the pane plugin's concrete
//! `OutputFanoutBuffer` type.
//!
//! The trait is intentionally narrow: the only required method is
//! `read_for_client`, which advances the per-client cursor inside the
//! plugin's internal output buffer and returns any bytes that became
//! available since the last read. Cursor management is entirely the
//! plugin's responsibility.

use bmux_session_models::{ClientId, SessionId};
use std::sync::Arc;
use uuid::Uuid;

/// Result of a single `read_for_client` call.
///
/// - `bytes` is the newly-available output slice.
/// - `stream_start` / `stream_end` describe the absolute byte range
///   within the pane's output stream that this slice covers (used by
///   tests + recording to order chunks).
/// - `stream_gap` is `true` when the client's previous cursor position
///   has been evicted from the output buffer's ring; the caller
///   should treat the returned bytes as discontinuous from the
///   previous read (and typically resync state).
#[derive(Debug, Clone, Default)]
pub struct OutputRead {
    pub bytes: Vec<u8>,
    pub stream_start: u64,
    pub stream_end: u64,
    pub stream_gap: bool,
}

impl OutputRead {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty() && !self.stream_gap
    }
}

/// Trait object implemented by the pane-runtime plugin; consumed by
/// server's `event_push_task` to pull bytes for a specific client
/// on demand.
pub trait PaneOutputReader: Send + Sync {
    /// Read up to `budget` bytes from the per-client cursor for the
    /// given `(session, pane, client)` triple. Advances the cursor on
    /// success. Returns `None` when the triple doesn't resolve
    /// (session closed, pane gone, client not attached).
    fn read_for_client(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
        budget: usize,
    ) -> Option<OutputRead>;

    /// Register a new cursor for `client_id` on every pane in
    /// `session_id`, starting at the pane's current tail. Called on
    /// successful attach so the client doesn't receive historical
    /// output. Returns `true` when the session exists.
    fn register_client_at_tail(&self, session_id: SessionId, client_id: ClientId) -> bool;

    /// Drop every cursor belonging to `client_id` across all
    /// sessions. Called on detach / disconnect. No-op if the client
    /// is not registered.
    fn deregister_client(&self, client_id: ClientId);

    /// Whether the given `(session, pane)` pair has any unread bytes
    /// past the given `client_id`'s cursor. Used by the push loop to
    /// skip a `PaneOutputAvailable` event when a different client
    /// consumed the bytes first.
    fn has_unread(&self, session_id: SessionId, pane_id: Uuid, client_id: ClientId) -> bool;
}

/// Registry newtype wrapping an `Arc<dyn PaneOutputReader>`. Core
/// code looks it up via `bmux_plugin::PluginStateRegistry` to
/// reach pane output through the trait surface without naming the
/// concrete `OutputFanoutBuffer` type.
#[derive(Clone)]
pub struct PaneOutputReaderHandle(pub Arc<dyn PaneOutputReader>);

impl PaneOutputReaderHandle {
    #[must_use]
    pub fn new<R: PaneOutputReader + 'static>(reader: R) -> Self {
        Self(Arc::new(reader))
    }

    #[must_use]
    pub fn from_arc(reader: Arc<dyn PaneOutputReader>) -> Self {
        Self(reader)
    }

    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopPaneOutputReader)
    }
}

/// Default no-op impl returning `None` / `false` for every query.
/// Server registers this at startup so handle lookups always succeed;
/// the pane-runtime plugin overwrites it during `activate`.
#[derive(Debug, Default)]
pub struct NoopPaneOutputReader;

impl PaneOutputReader for NoopPaneOutputReader {
    fn read_for_client(
        &self,
        _session_id: SessionId,
        _pane_id: Uuid,
        _client_id: ClientId,
        _budget: usize,
    ) -> Option<OutputRead> {
        None
    }

    fn register_client_at_tail(&self, _session_id: SessionId, _client_id: ClientId) -> bool {
        false
    }

    fn deregister_client(&self, _client_id: ClientId) {}

    fn has_unread(&self, _session_id: SessionId, _pane_id: Uuid, _client_id: ClientId) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{NoopPaneOutputReader, OutputRead, PaneOutputReader, PaneOutputReaderHandle};
    use bmux_session_models::{ClientId, SessionId};
    use uuid::Uuid;

    #[test]
    fn noop_reader_returns_none_and_false() {
        let reader = NoopPaneOutputReader;
        let session_id = SessionId(Uuid::new_v4());
        let pane_id = Uuid::new_v4();
        let client_id = ClientId(Uuid::new_v4());
        assert!(
            reader
                .read_for_client(session_id, pane_id, client_id, 1024)
                .is_none()
        );
        assert!(!reader.register_client_at_tail(session_id, client_id));
        reader.deregister_client(client_id);
        assert!(!reader.has_unread(session_id, pane_id, client_id));
    }

    #[test]
    fn handle_wraps_reader_and_is_clonable() {
        let handle = PaneOutputReaderHandle::noop();
        let clone = handle.clone();
        assert!(
            clone
                .0
                .read_for_client(
                    SessionId(Uuid::new_v4()),
                    Uuid::new_v4(),
                    ClientId(Uuid::new_v4()),
                    1024,
                )
                .is_none()
        );
    }

    #[test]
    fn output_read_is_empty_when_no_bytes_and_no_gap() {
        let read = OutputRead::default();
        assert!(read.is_empty());

        let with_bytes = OutputRead {
            bytes: vec![1, 2, 3],
            ..OutputRead::default()
        };
        assert!(!with_bytes.is_empty());

        let with_gap = OutputRead {
            stream_gap: true,
            ..OutputRead::default()
        };
        assert!(!with_gap.is_empty());
    }
}
