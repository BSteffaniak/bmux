//! Private, bounded input state. Never use an ordinary text editor for secrets.
//!
//! This module owns no layout. Render only [`SensitiveInputState`] using normal
//! BMUX layout primitives; secret bytes have no selection or source projection.

use std::fmt;
use unicode_segmentation::UnicodeSegmentation;
use zeroize::{Zeroize, Zeroizing};

/// Maximum UTF-8 bytes accepted by a private control.
pub const MAX_PRIVATE_INPUT_BYTES: usize = 4096;

/// Fixed-capacity secret storage with no clone, serialization, or edit history.
/// All storage, including deleted bytes, is wiped on destruction.
pub struct SensitiveInputBuffer {
    bytes: Zeroizing<Box<[u8]>>,
    len: usize,
}

impl fmt::Debug for SensitiveInputBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SensitiveInputBuffer([REDACTED])")
    }
}

impl Default for SensitiveInputBuffer {
    fn default() -> Self {
        Self {
            bytes: Zeroizing::new(vec![0; MAX_PRIVATE_INPUT_BYTES].into_boxed_slice()),
            len: 0,
        }
    }
}

impl SensitiveInputBuffer {
    /// Append UTF-8 text atomically. Control characters and overflow are rejected.
    /// The caller must wipe its source storage after this call.
    pub fn insert(&mut self, text: &str) -> bool {
        if text.chars().any(char::is_control)
            || text.len() > MAX_PRIVATE_INPUT_BYTES.saturating_sub(self.len)
        {
            return false;
        }
        let end = self.len + text.len();
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        true
    }

    /// Remove the final extended grapheme and wipe its complete byte range.
    pub fn backspace(&mut self) {
        let Ok(text) = std::str::from_utf8(&self.bytes[..self.len]) else {
            self.clear();
            return;
        };
        let start = text
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(start, _)| start);
        self.bytes[start..self.len].zeroize();
        self.len = start;
    }

    /// Wipe the entire allocation, including any removed text.
    pub fn clear(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
    }

    /// Consume the control's input for one private delivery. Empty input is refused.
    /// The returned storage is zeroizing and must not enter logs or ordinary events.
    #[must_use]
    pub fn into_private_bytes(mut self) -> Option<PrivateInputBytes> {
        if self.len == 0 {
            return None;
        }
        let bytes = Zeroizing::new(self.bytes[..self.len].to_vec());
        self.clear();
        Some(PrivateInputBytes(bytes))
    }
}

/// One-shot owned bytes, deliberately neither cloneable nor serializable.
pub struct PrivateInputBytes(Zeroizing<Vec<u8>>);
impl fmt::Debug for PrivateInputBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateInputBytes([REDACTED])")
    }
}
impl PrivateInputBytes {
    /// Borrow only during a private write. Do not copy into ordinary transport payloads.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Transfer ownership into another zeroizing private transport type, without copying.
    /// The recipient must immediately adopt and wipe this allocation on every path.
    #[must_use]
    pub fn into_transport_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut *self.0)
    }
}

/// Secret-free, length-independent rendering state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveInputState {
    /// Capture suppression has been acknowledged; the control accepts private input.
    Active,
    /// Input has been consumed or discarded. Never reopen a closed attempt.
    Closed,
}

/// Local private controller. It has no ordinary snapshot, editor, clipboard, or undo API.
pub struct SensitiveInputController {
    buffer: SensitiveInputBuffer,
    state: SensitiveInputState,
}
impl fmt::Debug for SensitiveInputController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SensitiveInputController")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}
impl SensitiveInputController {
    /// Construct only after the input owner has established capture suppression.
    /// The ingress lease owns this controller; applications should not feed ordinary events.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: SensitiveInputBuffer::default(),
            state: SensitiveInputState::Active,
        }
    }
    /// Length-independent presentation. A renderer should paint a fixed placeholder.
    #[must_use]
    pub const fn state(&self) -> SensitiveInputState {
        self.state
    }
    /// Append bounded text while active.
    pub fn insert(&mut self, text: &str) -> bool {
        self.state == SensitiveInputState::Active && self.buffer.insert(text)
    }
    /// Delete one grapheme while active.
    pub fn backspace(&mut self) {
        if self.state == SensitiveInputState::Active {
            self.buffer.backspace();
        }
    }
    /// Submit exactly once. Empty input leaves the control active.
    pub fn submit(&mut self) -> Option<PrivateInputBytes> {
        if self.state != SensitiveInputState::Active || self.buffer.len == 0 {
            return None;
        }
        self.state = SensitiveInputState::Closed;
        std::mem::take(&mut self.buffer).into_private_bytes()
    }
    /// Cancel, detach, time out, or shut down. All paths wipe input and are absorbing.
    pub fn close(&mut self) {
        self.buffer.clear();
        self.state = SensitiveInputState::Closed;
    }
}
impl Default for SensitiveInputController {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for SensitiveInputController {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_backspace_wipes_graphemes_without_editor_history() {
        let mut buffer = SensitiveInputBuffer::default();
        assert!(buffer.insert("界👩‍👩‍👧‍👦e\u{301}"));
        buffer.backspace();
        buffer.backspace();
        assert_eq!(&buffer.bytes[..buffer.len], "界".as_bytes());
        assert!(buffer.bytes[buffer.len..].iter().all(|byte| *byte == 0));
        buffer.backspace();
        assert!(buffer.into_private_bytes().is_none());
    }
    #[test]
    fn insertion_is_atomic_bounded_and_control_free() {
        let mut buffer = SensitiveInputBuffer::default();
        assert!(buffer.insert(&"x".repeat(MAX_PRIVATE_INPUT_BYTES)));
        assert!(!buffer.insert("界"));
        assert!(!buffer.insert("\n"));
        assert_eq!(
            buffer.into_private_bytes().unwrap().expose().len(),
            MAX_PRIVATE_INPUT_BYTES
        );
    }
    #[test]
    fn public_state_and_debug_do_not_reveal_content_or_length() {
        let mut controller = SensitiveInputController::new();
        let before = format!("{controller:?}");
        controller.insert("synthetic-private-input");
        assert_eq!(format!("{controller:?}"), before);
        assert_eq!(controller.state(), SensitiveInputState::Active);
        let bytes = controller.submit().unwrap();
        assert_eq!(format!("{bytes:?}"), "PrivateInputBytes([REDACTED])");
        assert!(controller.submit().is_none());
        assert!(!controller.insert("late"));
    }
    #[test]
    fn every_teardown_is_absorbing_and_wipes_storage() {
        let mut controller = SensitiveInputController::new();
        controller.insert("synthetic-private-input");
        controller.close();
        assert!(controller.buffer.bytes.iter().all(|byte| *byte == 0));
        controller.close();
        assert!(controller.submit().is_none());
        assert!(!controller.insert("late"));
    }
}
