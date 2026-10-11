//! Owning-PTY capture negotiation for trusted cooperating terminals.
//! This is not cryptographic attestation and does not protect external recorders.

use crate::attach_runtime_commands::InputCaptureAckV1;
use uuid::Uuid;

/// Public control prefix, never a password prompt or secret payload.
pub const CAPTURE_ACK_PREFIX: &str = "bmux-input-capture-v1:";

/// Create a versioned, nonce-correlated request for the actual containing pane.
/// Write and flush it before private focus; do not infer success from writing.
#[must_use]
pub fn request(nonce: Uuid) -> String {
    format!("\x1b]633;bmux-input-capture-v1;{nonce}\x07")
}

/// Validate the owning-PTY's bounded public response before private activation.
/// Wrong nonce, unknown fields, unsupported versions and missing suppression fail closed.
#[must_use]
pub fn acknowledgment(text: &str, nonce: Uuid) -> Option<InputCaptureAckV1> {
    if text.len() > 512 {
        return None;
    }
    let json = text.strip_prefix(CAPTURE_ACK_PREFIX)?;
    let object: serde_json::Value = serde_json::from_str(json).ok()?;
    let fields = object.as_object()?;
    if fields.len() != 3
        || !fields.contains_key("nonce")
        || !fields.contains_key("pane_id")
        || !fields.contains_key("recording_suppressed")
    {
        return None;
    }
    let ack: InputCaptureAckV1 = serde_json::from_value(object).ok()?;
    (ack.nonce == nonce && ack.recording_suppressed && !ack.pane_id.is_nil()).then_some(ack)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrong_nonce_unsupported_and_unsuppressed_acks_fail_closed() {
        let nonce = Uuid::new_v4();
        let ack = InputCaptureAckV1 {
            nonce,
            pane_id: Uuid::new_v4(),
            recording_suppressed: true,
        };
        let text = format!(
            "{CAPTURE_ACK_PREFIX}{}",
            serde_json::to_string(&ack).unwrap()
        );
        assert!(acknowledgment(&text, nonce).is_some());
        assert!(acknowledgment(&text, Uuid::new_v4()).is_none());
        assert!(acknowledgment(&text.replace("true", "false"), nonce).is_none());
        assert!(acknowledgment(&text.replace("v1:", "v2:"), nonce).is_none());
        assert!(
            acknowledgment(&format!("{CAPTURE_ACK_PREFIX}{}", "x".repeat(513)), nonce).is_none()
        );
    }
}
