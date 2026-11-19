//! Text-only OS clipboard ownership, separate from selection and terminal queries.
//!
//! Keep one instance alive for the native application lifetime. On Linux the
//! owner must remain alive to serve selections; closing a window must not drop
//! the application's clipboard. Persistence after process exit depends on the
//! desktop clipboard manager. Errors never include clipboard contents.

/// Maximum accepted UTF-8 clipboard payload (8 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

/// Clipboard failure without backend strings or sensitive payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardError {
    /// No available OS clipboard service.
    Unavailable,
    /// Clipboard currently contains no readable text.
    NoText,
    /// Payload exceeds the explicit transfer budget.
    TooLarge,
    /// Clipboard was explicitly closed.
    Closed,
}

/// Persistent native text clipboard; not an in-memory fallback or query owner.
///
/// Construct on the application event-loop thread and retain across windows.
/// Reads/writes must be requested by the application's existing clipboard path;
/// this adapter does not observe selection, synthesize paste, or own OSC replies.
/// Deliberately has no `Debug` implementation to avoid exposing backend state.
pub struct Clipboard {
    closed: bool,
}

impl Clipboard {
    /// Create a handle without contacting the OS; availability is checked on use.
    pub fn new() -> Result<Self, ClipboardError> {
        Ok(Self { closed: false })
    }

    /// Read UTF-8 text. Oversized text is rejected, never silently truncated.
    ///
    /// The OS/backend allocates the incoming payload before its length can be
    /// checked; this is a transfer limit, not a hostile-server allocation limit.
    pub fn read_text(&mut self) -> Result<String, ClipboardError> {
        if self.closed {
            return Err(ClipboardError::Closed);
        }
        native_read_result(bmux_clipboard::native_text::read())
    }

    /// Publish text without retaining a second in-process copy.
    pub fn write_text(&mut self, text: String) -> Result<(), ClipboardError> {
        validate_text(&text)?;
        if self.closed {
            return Err(ClipboardError::Closed);
        }
        bmux_clipboard::native_text::write(&text).map_err(|_| ClipboardError::Unavailable)
    }

    /// Close this handle during application shutdown. The shared native backend
    /// retains process-owned selections independently of individual handles.
    pub fn close(&mut self) {
        self.closed = true;
    }
}

fn native_read_result(
    result: Result<Option<String>, bmux_clipboard::ClipboardError>,
) -> Result<String, ClipboardError> {
    let text = result
        .map_err(|_| ClipboardError::Unavailable)?
        .ok_or(ClipboardError::NoText)?;
    validate_text(&text)?;
    Ok(text)
}

fn validate_text(text: &str) -> Result<(), ClipboardError> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        Err(ClipboardError::TooLarge)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_limit_is_utf8_bytes_and_never_truncates() {
        assert_eq!(validate_text(&"x".repeat(MAX_CLIPBOARD_BYTES)), Ok(()));
        assert_eq!(
            validate_text(&"é".repeat(MAX_CLIPBOARD_BYTES / 2 + 1)),
            Err(ClipboardError::TooLarge)
        );
        assert_eq!(validate_text(""), Ok(()));
    }

    #[test]
    fn native_reads_distinguish_absence_from_empty_and_redact_backend_errors() {
        assert_eq!(native_read_result(Ok(None)), Err(ClipboardError::NoText));
        for text in ["", "é\n"] {
            assert_eq!(native_read_result(Ok(Some(text.into()))), Ok(text.into()));
        }
        let err = bmux_clipboard::ClipboardError::BackendFailed {
            backend: "private".into(),
            message: "sensitive".into(),
        };
        assert_eq!(
            native_read_result(Err(err)),
            Err(ClipboardError::Unavailable)
        );
    }

    #[test]
    fn oversized_write_rejected_before_native_io() {
        let mut clipboard = Clipboard::new().unwrap();
        assert_eq!(
            clipboard.write_text("x".repeat(MAX_CLIPBOARD_BYTES + 1)),
            Err(ClipboardError::TooLarge)
        );
        clipboard.close();
        assert_eq!(clipboard.read_text(), Err(ClipboardError::Closed));
    }

    #[test]
    fn closed_owner_does_not_reopen_or_fall_back() {
        let mut clipboard = Clipboard::new().unwrap();
        clipboard.close();
        assert_eq!(clipboard.read_text(), Err(ClipboardError::Closed));
        assert_eq!(
            clipboard.write_text("private".into()),
            Err(ClipboardError::Closed)
        );
    }
}
