//! The host's audit log: pairings, revocations, role changes, and remote
//! connections (connected, disconnected, refused). Stored next to the trust
//! store in its redb database, bounded to [`CAPACITY`] entries (the oldest
//! are dropped). Entries never carry secrets: no pairing links or phrases,
//! only public device keys and untrusted display names.
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Most entries kept; older ones are dropped as new ones arrive.
pub const CAPACITY: u64 = 1_000;

/// Refusals of devices recorded per [`REFUSAL_WINDOW`]; more are dropped, so
/// unknown peers cannot turn handshakes into a stream of disk writes.
const REFUSALS_PER_WINDOW: u32 = 20;
const REFUSAL_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Paired,
    PairRejected,
    Revoked,
    RoleChanged,
    Connected,
    Disconnected,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// Unix seconds.
    pub at: u64,
    pub kind: Kind,
    pub device_key: Option<String>,
    pub device_name: Option<String>,
    pub role: Option<crate::trust::Role>,
    pub detail: Option<String>,
}

impl Event {
    pub fn new(kind: Kind) -> Self {
        Self {
            at: crate::trust::now_secs(),
            kind,
            device_key: None,
            device_name: None,
            role: None,
            detail: None,
        }
    }

    /// This event about `device`.
    pub fn device(mut self, device: &crate::trust::Device) -> Self {
        self.device_key = Some(device.key.clone());
        self.device_name = Some(device.name.clone());
        self.role = Some(device.role);
        self
    }

    pub fn key(mut self, key: &str) -> Self {
        self.device_key = Some(key.to_owned());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Bounds how many refusals reach the log.
#[derive(Debug)]
pub struct RefusalLimiter {
    window: Mutex<(Instant, u32)>,
}

impl Default for RefusalLimiter {
    fn default() -> Self {
        Self {
            window: Mutex::new((Instant::now(), 0)),
        }
    }
}

impl RefusalLimiter {
    /// Whether one more refusal may be recorded now.
    pub fn admit(&self) -> bool {
        let mut window = self
            .window
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if window.0.elapsed() >= REFUSAL_WINDOW {
            *window = (Instant::now(), 0);
        }
        if window.1 >= REFUSALS_PER_WINDOW {
            return false;
        }
        window.1 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_bounded_per_window() {
        let limiter = RefusalLimiter::default();
        let admitted = (0..REFUSALS_PER_WINDOW * 2)
            .filter(|_| limiter.admit())
            .count();
        assert_eq!(admitted, REFUSALS_PER_WINDOW as usize);
    }

    #[test]
    fn kinds_serialize_as_their_names() {
        for (kind, name) in [
            (Kind::Paired, "paired"),
            (Kind::PairRejected, "pair-rejected"),
            (Kind::Revoked, "revoked"),
            (Kind::RoleChanged, "role-changed"),
            (Kind::Connected, "connected"),
            (Kind::Disconnected, "disconnected"),
            (Kind::Refused, "refused"),
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::from(name)
            );
        }
    }
}
