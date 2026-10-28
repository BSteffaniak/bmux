//! Volatile, bounded observability, independent of authority and wire state.
use bmux_live_relay::IngestReason;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Publisher,
    Viewer,
}
impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::Publisher => "publisher",
            Self::Viewer => "viewer",
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disconnect {
    PeerClosed,
    Cancelled,
    TransportError,
    UnexpectedMessage,
    AuthorityChanged,
    PongTimeout,
    SendFailed,
    SendTimeout,
    AckTimeout,
    InvalidAck,
    InvalidControl,
    IngressThrottled,
    ViewerThrottled,
    AggregateThrottled,
    ControlThrottled,
    SnapshotFailed,
    UpdatesClosed,
}
impl Disconnect {
    fn label(self) -> &'static str {
        match self {
            Self::PeerClosed => "peer_closed",
            Self::Cancelled => "cancelled",
            Self::SendTimeout => "send_timeout",
            Self::TransportError => "transport_error",
            Self::UnexpectedMessage => "unexpected_message",
            Self::AuthorityChanged => "authority_changed",
            Self::PongTimeout => "pong_timeout",
            Self::SendFailed => "send_failed",
            Self::AckTimeout => "ack_timeout",
            Self::InvalidAck => "invalid_ack",
            Self::InvalidControl => "invalid_control",
            Self::IngressThrottled => "ingress_throttled",
            Self::ViewerThrottled => "viewer_throttled",
            Self::AggregateThrottled => "aggregate_throttled",
            Self::ControlThrottled => "control_throttled",
            Self::SnapshotFailed => "snapshot_failed",
            Self::UpdatesClosed => "updates_closed",
        }
    }
}
#[derive(Serialize)]
struct Progress {
    role: Role,
    #[serde(skip)]
    connected: Option<Instant>,
    #[serde(skip)]
    sending: Option<Instant>,
    #[serde(skip)]
    sent: Option<Instant>,
    #[serde(skip)]
    ack: Option<Instant>,
    #[serde(skip)]
    received: Option<Instant>,
    received_revision: Option<u64>,
    #[serde(skip)]
    reply_sent: Option<Instant>,
    reply_revision: Option<u64>,
    repair_required: bool,
    pending_revision: Option<u64>,
    ack_revision: Option<u64>,
}
const HISTORY_LIMIT: usize = 64;
struct History {
    at: Instant,
    connection_id: Uuid,
    role: Role,
    event: &'static str,
    reason: &'static str,
}
#[derive(Default)]
struct Data {
    history: VecDeque<History>,
    connections: BTreeMap<Uuid, Progress>,
    reasons: BTreeMap<(&'static str, &'static str), u64>,
    admitted: Option<Instant>,
    admitted_revision: Option<u64>,
}
#[derive(Default)]
pub struct Observability(Mutex<Data>);
fn age(value: Option<Instant>) -> Option<u64> {
    value.map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
}
impl Observability {
    fn lock(&self) -> std::sync::MutexGuard<'_, Data> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn history(&self, id: Uuid, role: Role, event: &'static str, reason: &'static str) {
        let mut data = self.lock();
        if data.history.len() == HISTORY_LIMIT {
            data.history.pop_front();
        }
        data.history.push_back(History {
            at: Instant::now(),
            connection_id: id,
            role,
            event,
            reason,
        });
    }
    pub fn rejected(&self, reason: IngestReason) {
        self.reason("ingest_rejected", reason.label());
    }
    fn reason(&self, event: &'static str, reason: &'static str) {
        let mut data = self.lock();
        let count = data.reasons.entry((event, reason)).or_default();
        *count = count.saturating_add(1);
    }
    pub fn publisher_auth_denied(&self) {
        self.reason("auth", "publisher_denied");
    }
    pub fn metrics_auth_denied(&self) {
        self.reason("auth", "metrics_denied");
    }
    pub fn http_throttled(&self) {
        self.reason("http", "throttled");
    }
    pub fn http_timeout(&self) {
        self.reason("http", "timeout");
    }
    pub fn admitted(&self, revision: u64) {
        let mut data = self.lock();
        data.admitted = Some(Instant::now());
        data.admitted_revision = Some(revision);
    }
    pub fn metrics(&self) -> String {
        let data = self.lock();
        let mut result = String::from("# TYPE bmux_relay_events_total counter\n");
        for ((event, reason), count) in &data.reasons {
            result.push_str(&format!(
                "bmux_relay_events_total{{event=\"{event}\",reason=\"{reason}\"}} {count}\n"
            ));
        }
        result
    }
    pub fn diagnostics(&self) -> Value {
        let data = self.lock();
        let connections: Vec<_> = data.connections.iter().map(|(id, p)| {
            // No pending work means idle, regardless of how old the last frame is.
            let state = if p.sending.is_some_and(|t| t.elapsed() >= Duration::from_secs(10)) { "stalled" }
                else if p.sending.is_some() { "sending" }
                else if p.repair_required { "repair_required" }
                else if p.pending_revision.is_none() { "idle" }
                else if p.sent.is_some_and(|t| t.elapsed() >= Duration::from_secs(10)) { "stalled" }
                else { "awaiting_ack" };
            json!({"connection_id":id, "role":p.role, "state":state,
                "connected_age_ms":age(p.connected), "sending_age_ms":age(p.sending), "sent_age_ms":age(p.sent),
                "received_age_ms":age(p.received), "received_revision":p.received_revision.map(|r| r.to_string()),
                "reply_sent_age_ms":age(p.reply_sent), "reply_revision":p.reply_revision.map(|r| r.to_string()),
                "repair_required":p.repair_required,
                "ack_age_ms":age(p.ack), "pending_revision":p.pending_revision.map(|r| r.to_string()),
                "ack_revision":p.ack_revision.map(|r| r.to_string())})
        }).collect();
        let history: Vec<_> = data
            .history
            .iter()
            .map(|h| {
                json!({"age_ms":age(Some(h.at)),
            "connection_id":h.connection_id, "role":h.role, "event":h.event, "reason":h.reason})
            })
            .collect();
        json!({"history":history, "history_limit":HISTORY_LIMIT, "admitted_age_ms":age(data.admitted),
            "admitted_revision":data.admitted_revision.map(|r| r.to_string()), "connections":connections})
    }
    pub fn connection(self: &Arc<Self>, role: Role) -> Connection {
        let id = Uuid::new_v4();
        self.lock().connections.insert(
            id,
            Progress {
                role,
                connected: Some(Instant::now()),
                sending: None,
                sent: None,
                received: None,
                received_revision: None,
                reply_sent: None,
                reply_revision: None,
                repair_required: false,
                ack: None,
                pending_revision: None,
                ack_revision: None,
            },
        );
        eprintln!(
            "{}",
            json!({"event":"relay_connected", "role":role, "connection_id":id})
        );
        Connection {
            telemetry: self.clone(),
            id,
            role,
            reason: Disconnect::Cancelled,
            seen_rejections: 0,
        }
    }
}
pub struct Connection {
    telemetry: Arc<Observability>,
    id: Uuid,
    role: Role,
    reason: Disconnect,
    seen_rejections: u16,
}
impl Connection {
    pub fn exit(&mut self, reason: Disconnect) {
        self.reason = reason;
    }
    pub fn rejected(&mut self, reason: IngestReason) {
        self.telemetry.rejected(reason);
        self.telemetry
            .history(self.id, self.role, "ingest_rejected", reason.label());
        let index = IngestReason::ALL
            .iter()
            .position(|r| *r == reason)
            .expect("closed enum");
        let bit = 1_u16 << index;
        // One structured warning per bounded cause per connection, not per frame.
        if self.seen_rejections & bit == 0 {
            self.seen_rejections |= bit;
            eprintln!(
                "{}",
                json!({"event":"relay_repair_required", "connection_id":self.id,
                "role":self.role, "reason":reason})
            );
        }
    }
    pub fn publisher_received(&self, revision: Option<u64>, repair: bool) {
        if let Some(p) = self.telemetry.lock().connections.get_mut(&self.id) {
            p.received = Some(Instant::now());
            p.received_revision = revision;
            p.repair_required = repair;
        }
    }
    pub fn publisher_reply_sent(&self) {
        if let Some(p) = self.telemetry.lock().connections.get_mut(&self.id) {
            p.reply_sent = Some(Instant::now());
            p.reply_revision = p.received_revision;
        }
    }
    pub fn sending(&self, active: bool) {
        if let Some(p) = self.telemetry.lock().connections.get_mut(&self.id) {
            p.sending = active.then(Instant::now);
        }
    }
    pub fn sent(&self, revision: u64) {
        if let Some(p) = self.telemetry.lock().connections.get_mut(&self.id) {
            p.sent = Some(Instant::now());
            p.pending_revision = Some(revision);
        }
    }
    pub fn ack(&self, revision: u64) {
        if let Some(p) = self.telemetry.lock().connections.get_mut(&self.id) {
            p.ack = Some(Instant::now());
            p.ack_revision = Some(revision);
            p.pending_revision = None;
        }
    }
    pub fn reset(&self) {
        if let Some(p) = self.telemetry.lock().connections.get_mut(&self.id) {
            p.pending_revision = None;
        }
    }
    pub fn repair(&self) {
        self.telemetry.reason("repair", "viewer_requested");
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.telemetry.lock().connections.remove(&self.id);
        self.telemetry
            .history(self.id, self.role, "disconnected", self.reason.label());
        self.telemetry
            .reason(self.role.label(), self.reason.label());
        eprintln!(
            "{}",
            json!({"event":"relay_disconnected", "role":self.role,
            "connection_id":self.id, "reason":self.reason})
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_history_evicts_oldest_without_retaining_authority_or_payload() {
        let telemetry = Arc::new(Observability::default());
        let mut connection = telemetry.connection(Role::Publisher);
        for _ in 0..HISTORY_LIMIT + 5 {
            connection.rejected(IngestReason::Malformed);
        }
        assert_eq!(telemetry.lock().history.len(), HISTORY_LIMIT);
        drop(connection);
        let diagnostics = telemetry.diagnostics();
        assert_eq!(
            diagnostics["history"].as_array().unwrap().len(),
            HISTORY_LIMIT
        );
        assert_eq!(
            diagnostics["history"][HISTORY_LIMIT - 1]["event"],
            "disconnected"
        );
        for private in ["epoch", "fence", "publisher_id", "payload", "key"] {
            assert!(!diagnostics.to_string().contains(private));
        }
    }

    #[test]
    fn idle_stalled_and_send_backpressure_are_distinct() {
        let telemetry = Arc::new(Observability::default());
        let connection = telemetry.connection(Role::Viewer);
        let state = || telemetry.diagnostics()["connections"][0]["state"].clone();
        assert_eq!(state(), "idle");
        connection.sent(3);
        assert_eq!(state(), "awaiting_ack");
        telemetry
            .lock()
            .connections
            .get_mut(&connection.id)
            .unwrap()
            .sent = Some(Instant::now() - Duration::from_secs(11));
        assert_eq!(state(), "stalled");
        connection.ack(3);
        assert_eq!(
            state(),
            "idle",
            "old progress without pending work is not a stall"
        );
        connection.sending(true);
        assert_eq!(state(), "sending");
        telemetry
            .lock()
            .connections
            .get_mut(&connection.id)
            .unwrap()
            .sending = Some(Instant::now() - Duration::from_secs(11));
        assert_eq!(state(), "stalled");
        connection.sending(false);
        connection.reset();
        assert_eq!(state(), "idle");
        drop(connection);
        assert!(
            telemetry.diagnostics()["connections"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(telemetry.metrics().contains("reason=\"cancelled\""));
    }

    #[test]
    fn rejection_cardinality_is_closed_and_connection_ids_are_private() {
        let telemetry = Arc::new(Observability::default());
        let mut connection = telemetry.connection(Role::Publisher);
        for _ in 0..2 {
            for reason in IngestReason::ALL {
                connection.rejected(reason);
            }
        }
        let metrics = telemetry.metrics();
        assert_eq!(
            metrics
                .lines()
                .filter(|line| !line.starts_with('#'))
                .count(),
            IngestReason::ALL.len()
        );
        assert!(!metrics.contains(&connection.id.to_string()));
        assert!(!metrics.contains("connection_id"));
        assert_eq!(
            connection.seen_rejections.count_ones() as usize,
            IngestReason::ALL.len()
        );
        connection.publisher_received(Some(8), false);
        connection.publisher_reply_sent();
        assert_eq!(
            telemetry.diagnostics()["connections"][0]["reply_revision"],
            "8"
        );
        // A socket write does not prove that the publisher received the ACK.
        assert!(telemetry.diagnostics()["connections"][0]["ack_revision"].is_null());
    }
}
