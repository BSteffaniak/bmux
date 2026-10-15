//! Bounded, credential-free serving evidence. Never accepts URLs, headers or bodies.
use serde::Serialize;
use std::{collections::VecDeque, sync::Mutex, time::Instant};

#[derive(Clone, Debug, Serialize)]
pub struct HostDiagnostics {
    pub running: bool,
    pub accepted_connections: u64,
    pub capacity_rejections: u64,
    pub request_rejections: u64,
    pub latest_publication_revision: Option<u64>,
    pub delivery: DeliveryCounters,
    pub events: Vec<HostEvent>,
}
/// Successful publication responses only; byte counters exclude HTTP headers.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DeliveryCounters {
    pub identity_responses: u64,
    pub gzip_responses: u64,
    pub identity_bytes: u64,
    pub transport_bytes: u64,
    pub full_responses: u64,
    pub delta_responses: u64,
    pub document_responses: u64,
    pub json_responses: u64,
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
    delivery: DeliveryCounters,
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
                delivery: DeliveryCounters::default(),
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
        // Diagnostics are queried through the host API or /diagnostics. Never
        // write to process stderr: it may share the presentation's terminal.
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
    pub(crate) fn delivered(
        &self,
        selected: &crate::delivery::Selected<'_>,
        representation: &'static str,
    ) {
        let mut evidence = self
            .evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counters = &mut evidence.delivery;
        let count = if selected.gzip {
            &mut counters.gzip_responses
        } else {
            &mut counters.identity_responses
        };
        *count = count.saturating_add(1);
        counters.identity_bytes = counters
            .identity_bytes
            .saturating_add(u64::try_from(selected.identity_len).unwrap_or(u64::MAX));
        counters.transport_bytes = counters
            .transport_bytes
            .saturating_add(u64::try_from(selected.bytes.len()).unwrap_or(u64::MAX));
        let count = match representation {
            "full" => &mut counters.full_responses,
            "delta" => &mut counters.delta_responses,
            "document" => &mut counters.document_responses,
            _ => &mut counters.json_responses,
        };
        *count = count.saturating_add(1);
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
            delivery: evidence.delivery.clone(),
            events: evidence.events.iter().cloned().collect(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn events_do_not_write_to_the_presenting_process_terminal() {
        const CHILD: &str = "BMUX_WEB_HOST_DIAGNOSTICS_CAPTURE_TEST";
        if std::env::var_os(CHILD).is_some() {
            let diagnostics = Diagnostics::new();
            diagnostics.delivered(
                &crate::delivery::Selected {
                    bytes: &[1, 2],
                    gzip: true,
                    identity_len: 4096,
                },
                "full",
            );
            assert_eq!(diagnostics.snapshot().delivery.transport_bytes, 2);
            assert_eq!(diagnostics.snapshot().delivery.identity_bytes, 4096);
            diagnostics.event("request_rejected", Some(408));
            diagnostics.event("response_write_failed", Some(32));
            diagnostics.stopped("listener_failed", None);
            assert_eq!(diagnostics.snapshot().events.len(), 3);
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::tests::events_do_not_write_to_the_presenting_process_terminal",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert!(
            output.stderr.is_empty(),
            "diagnostics leaked to stderr: {:?}",
            output.stderr
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("event="));
    }

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
