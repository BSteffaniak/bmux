//! Bounded pane-owned capture handshake carried over the owning PTY, not core IPC.
//! Public nonce and acknowledgment never contain private input.
use bmux_pane_runtime_plugin_api::attach_runtime_commands::InputCaptureAckV1;
use uuid::Uuid;

const PREFIX: &[u8] = b"\x1b]633;bmux-input-capture-v1;";
const MAX_REQUEST_BYTES: usize = 128;

#[derive(Default)]
pub(crate) struct CaptureHandshake {
    pending: Vec<u8>,
}

impl CaptureHandshake {
    /// Strip only complete, versioned control requests; ordinary output is preserved.
    #[cfg(test)]
    pub(crate) fn process(&mut self, bytes: &[u8], pane_id: Uuid) -> (Vec<u8>, Vec<u8>) {
        self.process_with_fence(bytes, pane_id, || false)
    }

    pub(crate) fn process_with_fence(
        &mut self,
        bytes: &[u8],
        pane_id: Uuid,
        mut settle_writes: impl FnMut() -> bool,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut output = Vec::with_capacity(bytes.len());
        let mut replies = Vec::new();
        for &byte in bytes {
            if self.pending.is_empty() && byte != PREFIX[0] {
                output.push(byte);
                continue;
            }
            self.pending.push(byte);
            if self.pending.len() <= PREFIX.len() && !PREFIX.starts_with(&self.pending) {
                output.append(&mut self.pending);
                continue;
            }
            if self.pending.len() > MAX_REQUEST_BYTES {
                output.append(&mut self.pending);
                continue;
            }
            if self.pending.len() > PREFIX.len() && byte == 7 {
                let payload = &self.pending[PREFIX.len()..self.pending.len() - 1];
                if let Ok(payload) = std::str::from_utf8(payload)
                    && let Some((phase, nonce)) = payload.split_once(':')
                    && matches!(phase, "enter-v2" | "exit-v2")
                    && let Ok(nonce) = Uuid::parse_str(nonce)
                {
                    let entering = phase == "enter-v2";
                    let generation = super::private_input::begin_barrier(pane_id, nonce, entering);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    let mut accepted = false;
                    if let Some(generation) = generation {
                        loop {
                            // Producer acknowledgments precede the FIFO fence. This
                            // never acknowledges bytes still queued for the PTY writer.
                            if super::private_input::barrier_ready(pane_id, nonce, generation) {
                                accepted = settle_writes()
                                    && super::private_input::finish_barrier(
                                        pane_id, nonce, generation,
                                    );
                                break;
                            }
                            if std::time::Instant::now() >= deadline {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                    }
                    replies.extend_from_slice(
                        format!(
                            "\x1b[200~bmux-input-capture-v2:{nonce}:{pane_id}:{phase}:{}\x1b[201~",
                            if accepted { "ok" } else { "denied" }
                        )
                        .as_bytes(),
                    );
                    self.pending.clear();
                    continue;
                }
                let nonce = std::str::from_utf8(payload)
                    .ok()
                    .and_then(|text| Uuid::parse_str(text).ok());
                if let Some(nonce) = nonce {
                    let ack = InputCaptureAckV1 {
                        nonce,
                        pane_id,
                        recording_suppressed: super::private_input::suppress(pane_id),
                    };
                    if let Ok(json) = serde_json::to_vec(&ack) {
                        // Wrapped as bracketed paste so the local private ingress can
                        // consume this secret-free control response before composer routing.
                        replies.extend_from_slice(b"\x1b[200~bmux-input-capture-v1:");
                        replies.extend_from_slice(&json);
                        replies.extend_from_slice(b"\x1b[201~");
                    }
                    self.pending.clear();
                } else {
                    output.append(&mut self.pending);
                }
            }
        }
        (output, replies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_owning_pty_request_returns_typed_nonce_ack_after_barrier() {
        let nonce = Uuid::new_v4();
        let pane = Uuid::new_v4();
        let request = format!("\x1b]633;bmux-input-capture-v1;{nonce}\x07");
        let mut parser = CaptureHandshake::default();
        let mut output = Vec::new();
        let mut replies = Vec::new();
        for byte in request.bytes() {
            let (public, reply) = parser.process(&[byte], pane);
            output.extend(public);
            replies.extend(reply);
        }
        assert_eq!(output, [] as [u8; 0]);
        let json = replies
            .strip_prefix(b"\x1b[200~bmux-input-capture-v1:")
            .unwrap()
            .strip_suffix(b"\x1b[201~")
            .unwrap();
        let ack: InputCaptureAckV1 = serde_json::from_slice(json).unwrap();
        assert_eq!(ack.nonce, nonce);
        assert_eq!(ack.pane_id, pane);
        assert!(ack.recording_suppressed);
        assert_eq!(
            super::super::private_input::with_capture_barrier(pane, |capture| capture),
            Some(false)
        );
    }
    #[test]
    fn unknown_and_oversized_requests_preserve_output_without_suppression() {
        let pane = Uuid::new_v4();
        for bytes in [
            b"ordinary\x1b[31mtext".to_vec(),
            format!("\x1b]633;bmux-input-capture-v1;{}\x07", "x".repeat(200)).into_bytes(),
        ] {
            let (output, replies) = CaptureHandshake::default().process(&bytes, pane);
            assert_eq!(output, bytes);
            assert_eq!(replies, [] as [u8; 0]);
        }
        assert_eq!(
            super::super::private_input::with_capture_barrier(pane, |capture| capture),
            Some(true)
        );
    }
}
