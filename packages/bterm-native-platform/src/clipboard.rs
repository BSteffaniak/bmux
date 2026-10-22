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
    backend: Option<arboard::Clipboard>,
}

impl Clipboard {
    /// Connect to the platform clipboard, failing explicitly if unavailable.
    pub fn new() -> Result<Self, ClipboardError> {
        let backend = arboard::Clipboard::new().map_err(|_| ClipboardError::Unavailable)?;
        Ok(Self {
            backend: Some(backend),
        })
    }

    /// Read UTF-8 text. Oversized text is rejected, never silently truncated.
    ///
    /// The OS/backend allocates the incoming payload before its length can be
    /// checked; this is a transfer limit, not a hostile-server allocation limit.
    pub fn read_text(&mut self) -> Result<String, ClipboardError> {
        let backend = self.backend.as_mut().ok_or(ClipboardError::Closed)?;
        let text = backend.get_text().map_err(|error| match error {
            arboard::Error::ContentNotAvailable => ClipboardError::NoText,
            _ => ClipboardError::Unavailable,
        })?;
        validate_text(&text)?;
        Ok(text)
    }

    /// Publish text without retaining a second in-process copy.
    pub fn write_text(&mut self, text: String) -> Result<(), ClipboardError> {
        validate_text(&text)?;
        self.backend
            .as_mut()
            .ok_or(ClipboardError::Closed)?
            .set_text(text)
            .map_err(|_| ClipboardError::Unavailable)
    }

    /// Release this process's clipboard owner during application shutdown.
    /// Does not clear another application's clipboard or pretend persistence.
    pub fn close(&mut self) {
        self.backend.take();
    }
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
    fn closed_owner_does_not_reopen_or_fall_back() {
        let mut clipboard = Clipboard { backend: None };
        clipboard.close();
        assert_eq!(clipboard.read_text(), Err(ClipboardError::Closed));
        assert_eq!(
            clipboard.write_text("private".into()),
            Err(ClipboardError::Closed)
        );
    }
}
