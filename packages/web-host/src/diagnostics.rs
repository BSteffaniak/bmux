//! Bounded, credential-free serving evidence. Never accepts URLs, headers or bodies.
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
    time::Instant,
};

#[derive(Clone, Debug, Serialize)]
pub struct HostDiagnostics {
    pub running: bool,
    pub accepted_connections: u64,
    pub capacity_rejections: u64,
    pub request_rejections: u64,
    pub latest_publication_revision: Option<u64>,
    pub events: Vec<HostEvent>,
}
#[derive(Clone, Debug, Serialize)]
pub struct HostEvent {
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub code: &'static str,
    pub detail: Option<u64>,
}
struct Evidence {
    running: bool,
    accepted: u64,
    capacity: u64,
    rejected: u64,
    sequence: u64,
    published_revision: Option<u64>,
    counts: BTreeMap<&'static str, u64>,
    events: VecDeque<HostEvent>,
}
pub(crate) struct Diagnostics {
    started: Instant,
    evidence: Mutex<Evidence>,
}
impl Diagnostics {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            evidence: Mutex::new(Evidence {
                running: true,
                accepted: 0,
                capacity: 0,
                rejected: 0,
                sequence: 0,
                published_revision: None,
                counts: BTreeMap::new(),
                events: VecDeque::new(),
            }),
        }
    }
    pub(crate) fn accepted(&self) {
        let mut evidence = self
            .evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        evidence.accepted = evidence.accepted.saturating_add(1);
    }
    pub(crate) fn event(&self, code: &'static str, detail: Option<u64>) {
        let mut evidence = self
            .evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = evidence.counts.entry(code).or_insert(0);
        *count = count.saturating_add(1);
        let count = *count;
        match code {
            "capacity_rejected" => {
                evidence.capacity = evidence.capacity.saturating_add(1);
            }
            "request_rejected" => {
                evidence.rejected = evidence.rejected.saturating_add(1);
            }
            _ => {}
        };
        evidence.sequence = evidence.sequence.saturating_add(1);
        let event = HostEvent {
            sequence: evidence.sequence,
            elapsed_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            code,
            detail,
        };
        if evidence.events.len() == 64 {
            evidence.events.pop_front();
        }
        evidence.events.push_back(event);
        // First and power-of-two repetitions retain evidence without flooding stderr.
        if count.is_power_of_two() {
            eprintln!("bmux_web_host event={code} detail={detail:?} count={count}");
        }
    }
    pub(crate) fn stopped(&self, code: &'static str, detail: Option<u64>) {
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running = false;
        self.event(code, detail);
    }
    pub(crate) fn published(&self, revision: u64) {
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .published_revision = Some(revision);
    }
    pub(crate) fn snapshot(&self) -> HostDiagnostics {
        let evidence = self
            .evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        HostDiagnostics {
            running: evidence.running,
            accepted_connections: evidence.accepted,
            capacity_rejections: evidence.capacity,
            request_rejections: evidence.rejected,
            latest_publication_revision: evidence.published_revision,
            events: evidence.events.iter().cloned().collect(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_is_bounded_but_counters_are_retained() {
        let diagnostics = Diagnostics::new();
        for _ in 0..100 {
            diagnostics.event("capacity_rejected", None);
        }
        diagnostics.accepted();
        diagnostics.stopped("producer_closed", None);
        let snapshot = diagnostics.snapshot();
        assert!(!snapshot.running);
        assert_eq!(snapshot.capacity_rejections, 100);
        assert_eq!(snapshot.accepted_connections, 1);
        assert_eq!(snapshot.events.len(), 64);
        assert_eq!(snapshot.events.last().unwrap().code, "producer_closed");
        assert_eq!(snapshot.events.last().unwrap().sequence, 101);
    }
}
