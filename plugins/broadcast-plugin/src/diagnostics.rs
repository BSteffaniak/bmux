//! Bounded process-local publisher observations. No endpoint, authority or peer text is retained.
use bmux_broadcast_plugin_api::broadcast_diagnostics_v1::{Event, Failure, Snapshot};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub(crate) const EVENT_LIMIT: usize = 256;
static PROCESS_INSTANCE: OnceLock<String> = OnceLock::new();
const STALL_AFTER: Duration = Duration::from_secs(6);
pub(crate) type Shared = Arc<Mutex<Observations>>;

#[derive(Default)]
pub(crate) struct Observations {
    sequence: u64,
    attempt: u64,
    events: VecDeque<(Instant, Event)>,
    current: BTreeMap<&'static str, (Instant, Failure)>,
    instance: OnceLock<String>,
    generation: Option<String>,
    ack_generation: Option<String>,
    send_generation: Option<String>,
    ack_latency_ms: Option<u64>,
    observer_state: Option<&'static str>,
    observer_exit: Option<&'static str>,
    observer_seen: Option<Instant>,
    transition: Option<(String, String)>,
    last: Option<(Instant, Failure)>,
    source_seen: Option<Instant>,
    poll_seen: Option<Instant>,
    source_polls: u64,
    source_revision: Option<u64>,
    context: u64,
    pending_revision: Option<u64>,
    pending_since: Option<Instant>,
    source_frames: u64,
    attempts: u64,
    acks: u64,
    repairs: u64,
    failures: u64,
    ack_seen: Option<Instant>,
    ack_revision: Option<u64>,
    sent_seen: Option<Instant>,
    sent_revision: Option<u64>,
    frames_sent: u64,
    full_frames: u64,
    delta_frames: u64,
    bytes_sent: u64,
}
fn age(at: Instant, now: Instant) -> u64 {
    u64::try_from(now.saturating_duration_since(at).as_millis()).unwrap_or(u64::MAX)
}
fn process_instance() -> &'static str {
    PROCESS_INSTANCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}
// Generation strings are source-controlled opaque values: never echo them into logs/diagnostics.
fn generation_fingerprint(generation: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut fingerprint = String::with_capacity(71);
    fingerprint.push_str("sha256:");
    for byte in Sha256::digest(generation.as_bytes()) {
        // Writing to a String is infallible.
        write!(fingerprint, "{byte:02x}").expect("String formatting is infallible");
    }
    fingerprint
}
impl Observations {
    fn event(
        &mut self,
        attempt_id: u64,
        operation: &str,
        kind: &str,
        reason: Option<&str>,
        revision: Option<u64>,
    ) {
        self.sequence = self.sequence.saturating_add(1);
        if self.events.len() == EVENT_LIMIT {
            self.events.pop_front();
        }
        self.events.push_back((
            Instant::now(),
            Event {
                sequence: self.sequence,
                process_instance: process_instance().into(),
                controller_instance: self.instance().into(),
                attempt_id,
                operation: operation.into(),
                kind: kind.into(),
                reason: reason.map(str::to_owned),
                revision,
                generation: self.generation.clone(),
                age_ms: 0,
                http_status: None,
                refresh: self.context.saturating_add(1),
            },
        ));
    }
    fn instance(&self) -> &str {
        self.instance
            .get_or_init(|| uuid::Uuid::new_v4().to_string())
    }
    pub(crate) fn transition(&mut self, local: &str, remote: &str) {
        let next = (local.to_owned(), remote.to_owned());
        if self.transition.as_ref() != Some(&next) {
            self.transition = Some(next);
            tracing::info!(target: "bmux.broadcast.diagnostics", process_instance = process_instance(), controller_instance = self.instance(), refresh = self.context.saturating_add(1), local, remote, "publisher state transition");
            self.event(0, "controller", "transition", None, None);
        }
    }
    pub(crate) fn observer(&mut self, state: &'static str, reason: Option<&'static str>) {
        self.observer_state = Some(state);
        self.observer_exit = reason;
        self.observer_seen = Some(Instant::now());
        self.event(0, "observer", state, reason, None);
        tracing::info!(target: "bmux.broadcast.diagnostics", process_instance = process_instance(), controller_instance = self.instance(), state, reason, "publisher observer transition");
        if state == "running" {
            self.current.remove("observer");
        } else if let Some(reason) = reason {
            let failure = Failure {
                attempt_id: 0,
                operation: "observer".into(),
                reason: reason.into(),
                age_ms: 0,
            };
            self.current
                .insert("observer", (Instant::now(), failure.clone()));
            self.last = Some((Instant::now(), failure));
        }
    }
    pub(crate) fn generation(&mut self, generation: &str) {
        self.generation = Some(generation_fingerprint(generation));
    }
    pub(crate) fn poll(&mut self) {
        self.poll_seen = Some(Instant::now());
        self.source_polls = self.source_polls.saturating_add(1);
    }
    pub(crate) fn source(&mut self, revision: u64, duplicate: bool) {
        self.source_seen = Some(Instant::now());
        self.source_revision = Some(revision);
        self.source_frames = self.source_frames.saturating_add(1);
        if !duplicate {
            self.pending_revision = Some(revision);
            self.pending_since.get_or_insert_with(Instant::now);
        }
    }
    pub(crate) fn invalidate(&mut self) {
        self.context = self.context.saturating_add(1);
        self.pending_revision = None;
        self.pending_since = None;
        self.source_revision = None;
        self.ack_revision = None;
        self.ack_seen = None;
        self.sent_seen = None;
        self.sent_revision = None;
        self.source_seen = None;
        self.poll_seen = None;
        self.current.clear();
        self.generation = None;
        self.ack_generation = None;
        self.send_generation = None;
        self.ack_latency_ms = None;
        self.observer_state = None;
        self.observer_exit = None;
        self.observer_seen = None;
    }
    pub(crate) fn events(&self, last: u32) -> Vec<Event> {
        let now = Instant::now();
        self.events
            .iter()
            .skip(
                self.events.len().saturating_sub(
                    usize::try_from(last)
                        .unwrap_or(EVENT_LIMIT)
                        .min(EVENT_LIMIT),
                ),
            )
            .map(|(at, event)| {
                let mut event = event.clone();
                event.age_ms = age(*at, now);
                event
            })
            .collect()
    }
    pub(crate) fn snapshot(
        &self,
        local: &str,
        remote: &str,
        refresh: u64,
        suppressed: u64,
        coalesced: u64,
    ) -> Snapshot {
        let now = Instant::now();
        let pending = self.pending_since.map(|at| age(at, now));
        let progress = if local != "running" {
            local
        } else if pending
            .is_some_and(|ms| ms >= u64::try_from(STALL_AFTER.as_millis()).unwrap_or(u64::MAX))
        {
            "stalled"
        } else if pending.is_some() {
            "pending"
        } else if self.ack_seen.is_some() {
            "idle"
        } else {
            "awaiting_source"
        };
        let failure = |value: &Option<(Instant, Failure)>| {
            value.as_ref().map(|(at, f)| {
                let mut f = f.clone();
                f.age_ms = age(*at, now);
                f
            })
        };
        Snapshot {
            local: local.into(),
            remote: remote.into(),
            progress: progress.into(),
            refresh,
            process_instance: process_instance().into(),
            controller_instance: self.instance().into(),
            current_failure: self
                .current
                .values()
                .max_by_key(|(at, _)| *at)
                .map(|(at, f)| {
                    let mut f = f.clone();
                    f.age_ms = age(*at, now);
                    f
                }),
            current_failures: self
                .current
                .values()
                .map(|(at, f)| {
                    let mut f = f.clone();
                    f.age_ms = age(*at, now);
                    f
                })
                .collect(),
            source_generation: self.generation.clone(),
            ack_generation: self.ack_generation.clone(),
            send_generation: self.send_generation.clone(),
            ack_latency_ms: self.ack_latency_ms,
            observer_state: self.observer_state.map(str::to_owned),
            observer_exit: self.observer_exit.map(str::to_owned),
            observer_age_ms: self.observer_seen.map(|at| age(at, now)),
            source_poll_age_ms: self.poll_seen.map(|at| age(at, now)),
            source_polls: self.source_polls,
            local_error_code: None,
            source_lease_remaining_ms: None,
            last_failure: failure(&self.last),
            source_age_ms: self.source_seen.map(|at| age(at, now)),
            source_revision: self.source_revision,
            ack_age_ms: self.ack_seen.map(|at| age(at, now)),
            ack_revision: self.ack_revision,
            send_age_ms: self.sent_seen.map(|at| age(at, now)),
            send_revision: self.sent_revision,
            frames_sent: self.frames_sent,
            full_frames: self.full_frames,
            delta_frames: self.delta_frames,
            bytes_sent: self.bytes_sent,
            pending_age_ms: pending,
            source_frames: self.source_frames,
            attempts: self.attempts,
            acknowledgements: self.acks,
            repairs: self.repairs,
            failures: self.failures,
            suppressed,
            coalesced,
            latest_event: self.sequence,
            history_capacity: 256,
            stall_after_ms: u64::try_from(STALL_AFTER.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// Cancellation is observable too; taking the socket still invalidates its delta baseline.
pub(crate) struct Attempt {
    shared: Shared,
    id: u64,
    operation: &'static str,
    revision: Option<u64>,
    context: u64,
    finished: bool,
    started: Instant,
    generation: Option<String>,
}
impl Attempt {
    pub(crate) fn begin(shared: &Shared, operation: &'static str, revision: Option<u64>) -> Self {
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.attempt = state.attempt.saturating_add(1);
        state.attempts = state.attempts.saturating_add(1);
        let id = state.attempt;
        state.event(id, operation, "attempt", None, revision);
        Self {
            shared: shared.clone(),
            id,
            operation,
            revision,
            context: state.context,
            finished: false,
            started: Instant::now(),
            generation: None,
        }
    }
    pub(crate) fn generation(&mut self, generation: &str) {
        self.generation = Some(generation_fingerprint(generation));
        if let Ok(mut state) = self.shared.lock()
            && let Some((_, event)) = state.events.back_mut()
            && event.attempt_id == self.id
        {
            event.generation = self.generation.clone();
        }
    }
    pub(crate) fn http_status(&self, status: u16) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.event(
            self.id,
            self.operation,
            "http_response",
            None,
            self.revision,
        );
        if let Some((_, event)) = state.events.back_mut() {
            event.http_status = Some(u32::from(status));
            event.generation = self.generation.clone();
            event.refresh = self.context.saturating_add(1);
        }
    }
    pub(crate) fn sent(&self, bytes: usize, full: bool) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frames_sent = state.frames_sent.saturating_add(1);
        state.bytes_sent = state
            .bytes_sent
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if full {
            state.full_frames = state.full_frames.saturating_add(1);
        } else {
            state.delta_frames = state.delta_frames.saturating_add(1);
        }
        if state.context == self.context {
            state.sent_seen = Some(Instant::now());
            state.sent_revision = self.revision;
            state.send_generation = self.generation.clone();
        }
        state.event(
            self.id,
            self.operation,
            if full { "full_sent" } else { "delta_sent" },
            None,
            self.revision,
        );
        if let Some((_, event)) = state.events.back_mut() {
            event.generation = self.generation.clone();
            event.refresh = self.context.saturating_add(1);
        }
    }
    pub(crate) fn peer_repair(&self, reason: &'static str) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.event(
            self.id,
            self.operation,
            "relay_repair_reason",
            Some(reason),
            self.revision,
        );
        tracing::warn!(target: "bmux.broadcast.diagnostics", process_instance = process_instance(), controller_instance = state.instance(), attempt_id = self.id, reason, "relay requested frame repair");
    }
    pub(crate) fn repair(&self) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.repairs = state.repairs.saturating_add(1);
        state.event(self.id, self.operation, "repair", None, self.revision);
        if let Some((_, event)) = state.events.back_mut() {
            event.generation = self.generation.clone();
            event.refresh = self.context.saturating_add(1);
        }
    }
    pub(crate) fn finish(mut self, result: Result<(), &'static str>) -> Result<(), &'static str> {
        self.record(result.err());
        self.finished = true;
        result
    }
    fn record(&self, reason: Option<&'static str>) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reason) = reason {
            state.failures = state.failures.saturating_add(1);
            let failure = Failure {
                attempt_id: self.id,
                operation: self.operation.into(),
                reason: reason.into(),
                age_ms: 0,
            };
            if state.context == self.context {
                state
                    .current
                    .insert(self.operation, (Instant::now(), failure.clone()));
            }
            state.last = Some((Instant::now(), failure));
            tracing::warn!(target: "bmux.broadcast.diagnostics", process_instance = process_instance(), controller_instance = state.instance(), attempt_id = self.id, operation = self.operation, refresh = self.context.saturating_add(1), revision = self.revision, reason, "publisher attempt failed");
        } else if state.context == self.context {
            // Only evidence from this operation resolves its own failure stage.
            if (self.operation != "frame" || state.pending_revision == self.revision)
                && state.current.remove(self.operation).is_some()
            {
                tracing::info!(target: "bmux.broadcast.diagnostics", process_instance = process_instance(), controller_instance = state.instance(), attempt_id = self.id, operation = self.operation, "publisher stage recovered");
            }
            if self.operation == "frame" {
                state.acks = state.acks.saturating_add(1);
                state.ack_seen = Some(Instant::now());
                state.ack_revision = self.revision;
                state.ack_generation = self.generation.clone();
                state.ack_latency_ms = Some(age(self.started, Instant::now()));
                if state.pending_revision == self.revision {
                    state.pending_revision = None;
                    state.pending_since = None;
                }
            }
        }
        state.event(
            self.id,
            self.operation,
            if reason.is_some() {
                "failure"
            } else {
                "success"
            },
            reason,
            self.revision,
        );
        if let Some((_, event)) = state.events.back_mut() {
            event.refresh = self.context.saturating_add(1);
            event.generation = self.generation.clone();
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.finished {
            self.record(Some("attempt_cancelled"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(s: &Observations) -> Snapshot {
        s.snapshot("running", "live", 1, 0, 0)
    }
    #[test]
    fn idle_is_not_stalled_and_failure_survives_heartbeat() {
        let shared = Shared::default();
        shared.lock().unwrap().source(5, false);
        let frame = Attempt::begin(&shared, "frame", Some(5));
        frame.finish(Err("ack_timeout")).unwrap_err();
        Attempt::begin(&shared, "heartbeat", None)
            .finish(Ok(()))
            .unwrap();
        assert_eq!(
            snapshot(&shared.lock().unwrap())
                .current_failure
                .unwrap()
                .reason,
            "ack_timeout"
        );
        shared.lock().unwrap().pending_since = Some(Instant::now() - Duration::from_secs(7));
        assert_eq!(snapshot(&shared.lock().unwrap()).progress, "stalled");
        let frame = Attempt::begin(&shared, "frame", Some(5));
        frame.repair();
        frame.finish(Ok(())).unwrap();
        shared.lock().unwrap().ack_seen = Some(Instant::now() - Duration::from_secs(70));
        let s = snapshot(&shared.lock().unwrap());
        assert_eq!(s.progress, "idle");
        assert!(s.current_failure.is_none());
        assert_eq!(s.last_failure.unwrap().reason, "ack_timeout");
        assert_eq!(s.repairs, 1);
    }
    #[test]
    fn superseded_ack_and_invalidated_attempt_do_not_hide_pending_work() {
        let shared = Shared::default();
        shared.lock().unwrap().source(1, false);
        let old = Attempt::begin(&shared, "frame", Some(1));
        shared.lock().unwrap().source(2, false);
        old.finish(Ok(())).unwrap();
        assert_eq!(snapshot(&shared.lock().unwrap()).progress, "pending");
        let cancelled = Attempt::begin(&shared, "frame", Some(2));
        shared.lock().unwrap().invalidate();
        drop(cancelled);
        let s = snapshot(&shared.lock().unwrap());
        assert!(s.current_failure.is_none());
        assert!(s.source_revision.is_none());
        assert_eq!(s.last_failure.unwrap().reason, "attempt_cancelled");
        assert_eq!(s.progress, "awaiting_source");
    }
    #[test]
    fn stage_failures_coexist_and_only_matching_success_resolves_them() {
        let shared = Shared::default();
        shared.lock().unwrap().source(1, false);
        Attempt::begin(&shared, "frame", Some(1))
            .finish(Err("ack_timeout"))
            .unwrap_err();
        Attempt::begin(&shared, "heartbeat", None)
            .finish(Err("http_timeout"))
            .unwrap_err();
        assert_eq!(snapshot(&shared.lock().unwrap()).current_failures.len(), 2);
        Attempt::begin(&shared, "heartbeat", None)
            .finish(Ok(()))
            .unwrap();
        let s = snapshot(&shared.lock().unwrap());
        assert_eq!(s.current_failures.len(), 1);
        assert_eq!(s.current_failures[0].operation, "frame");
        let mut frame = Attempt::begin(&shared, "frame", Some(1));
        frame.generation("untrusted-generation\nPRIVATE");
        frame.sent(100, true);
        frame.finish(Ok(())).unwrap();
        let s = snapshot(&shared.lock().unwrap());
        assert!(s.current_failures.is_empty());
        assert!(s.ack_latency_ms.is_some());
        assert!(s.ack_generation.unwrap().starts_with("sha256:"));
        assert!(
            !serde_json::to_string(&shared.lock().unwrap().events(50))
                .unwrap()
                .contains("PRIVATE")
        );
    }
    #[test]
    fn observer_and_instance_evidence_is_explicit() {
        let mut first = Observations::default();
        first.observer("running", None);
        first.observer("exited", Some("observer_failed"));
        let a = snapshot(&first);
        let b = snapshot(&Observations::default());
        assert_eq!(a.process_instance, b.process_instance);
        assert_ne!(a.controller_instance, b.controller_instance);
        assert_eq!(a.observer_exit.as_deref(), Some("observer_failed"));
        assert_eq!(a.current_failures[0].operation, "observer");
        assert!(a.observer_age_ms.is_some());
    }
    #[test]
    fn history_is_bounded_correlated_and_safe() {
        let shared = Shared::default();
        for _ in 0..300 {
            drop(Attempt::begin(&shared, "frame", Some(1)));
        }
        let state = shared.lock().unwrap();
        let events = state.events(u32::MAX);
        assert_eq!(events.len(), EVENT_LIMIT);
        assert_eq!(state.events(50).len(), 50);
        assert_eq!(events.last().unwrap().attempt_id, 300);
        let json = serde_json::to_string(&events).unwrap();
        assert!(!json.contains("credential"));
        assert!(!json.contains("fence"));
        assert!(state.events(0).is_empty());
    }
}
