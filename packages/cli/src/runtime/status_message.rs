use std::time::{Duration, Instant};

const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(3);

pub(super) struct StatusMessage {
    pub(super) text: String,
    expires_at: Instant,
}

impl StatusMessage {
    pub(super) fn new(text: String) -> Self {
        Self {
            text,
            expires_at: Instant::now() + STATUS_MESSAGE_TTL,
        }
    }
}

pub(super) fn is_expired(message: &StatusMessage) -> bool {
    Instant::now() >= message.expires_at
}
