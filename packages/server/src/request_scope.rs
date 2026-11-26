//! What one client request carries into everything it runs: the time its
//! client stops waiting, and a record of the nested calls it is waiting on.
//!
//! A connection starts a root scope per request; service contexts capture the
//! current scope when they are built, and nested kernel requests run inside
//! the scope of the context that issued them. Everything here is generic:
//! nested calls are named only by their service interface and operation.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

tokio::task_local! {
    static CURRENT: RequestScope;
}

/// The deadline and nested-call trace of the client request being served.
#[derive(Clone, Default)]
pub struct RequestScope {
    deadline: Option<Instant>,
    trace: Arc<NestedCallTrace>,
}

impl std::fmt::Debug for RequestScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestScope")
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl RequestScope {
    /// A new scope for one client request.
    pub(crate) fn root(deadline: Option<Instant>) -> Self {
        Self {
            deadline,
            trace: Arc::default(),
        }
    }

    /// The scope of the request being served on this task, or an empty one
    /// (no deadline) outside any request.
    pub(crate) fn current() -> Self {
        CURRENT.try_with(Clone::clone).unwrap_or_default()
    }

    /// Run `future` inside this scope.
    pub(crate) async fn run<F: Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }

    /// This scope with another deadline; its trace is kept.
    pub(crate) const fn with_deadline(mut self, deadline: Option<Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    /// When the client stops waiting for the reply, if it said.
    #[must_use]
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Record a nested call this request is waiting on, until the guard drops.
    pub(crate) fn enter_nested(&self, label: String) -> NestedCallGuard {
        let id = self.trace.enter(label);
        NestedCallGuard {
            trace: Arc::clone(&self.trace),
            id,
        }
    }

    /// The nested calls still running, oldest (outermost) first, with how
    /// long each has run: `a.op 5012ms > b.op 4990ms`. Empty when none.
    pub(crate) fn describe_waits(&self) -> String {
        self.trace.describe_in_flight(Instant::now())
    }

    /// The slowest nested call that finished, with how long it took.
    pub(crate) fn describe_slowest(&self) -> String {
        self.trace.describe_slowest()
    }
}

/// Removes a nested call from its trace when the call finishes.
pub struct NestedCallGuard {
    trace: Arc<NestedCallTrace>,
    id: u64,
}

impl Drop for NestedCallGuard {
    fn drop(&mut self) {
        self.trace.exit(self.id);
    }
}

#[derive(Default)]
struct NestedCallTrace {
    state: Mutex<TraceState>,
}

#[derive(Default)]
struct TraceState {
    next_id: u64,
    in_flight: BTreeMap<u64, (String, Instant)>,
    slowest: Option<(String, Duration)>,
}

impl NestedCallTrace {
    fn enter(&self, label: String) -> u64 {
        let Ok(mut state) = self.state.lock() else {
            return u64::MAX;
        };
        let id = state.next_id;
        state.next_id += 1;
        state.in_flight.insert(id, (label, Instant::now()));
        id
    }

    fn exit(&self, id: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some((label, started)) = state.in_flight.remove(&id) {
            let took = started.elapsed();
            if state
                .slowest
                .as_ref()
                .is_none_or(|(_, slowest)| took > *slowest)
            {
                state.slowest = Some((label, took));
            }
        }
    }

    fn describe_in_flight(&self, now: Instant) -> String {
        let Ok(state) = self.state.lock() else {
            return String::new();
        };
        state
            .in_flight
            .values()
            .map(|(label, started)| {
                format!(
                    "{label} {}ms",
                    now.saturating_duration_since(*started).as_millis()
                )
            })
            .collect::<Vec<_>>()
            .join(" > ")
    }

    fn describe_slowest(&self) -> String {
        let Ok(state) = self.state.lock() else {
            return String::new();
        };
        state
            .slowest
            .as_ref()
            .map(|(label, took)| format!("{label} {}ms", took.as_millis()))
            .unwrap_or_default()
    }
}

/// When a still-running service invocation is reported first; later reports
/// come at each doubling of that, and once when its deadline passes.
const FIRST_SLOW_REPORT: Duration = Duration::from_secs(1);

/// Await a service invocation, reporting (by interface and operation) the
/// nested calls it is waiting on whenever it runs long or past its deadline.
/// Never cancels it: a started invocation may have effects in progress.
pub async fn watch_invocation<F: Future>(
    interface_id: &str,
    operation: &str,
    invocation: F,
) -> F::Output {
    let scope = RequestScope::current();
    let started = Instant::now();
    let mut invocation = std::pin::pin!(invocation);
    let mut next_report = FIRST_SLOW_REPORT;
    let mut deadline_reported = false;
    loop {
        let report_at = started + next_report;
        let wake = match scope.deadline {
            Some(deadline) if !deadline_reported && deadline < report_at => deadline,
            _ => report_at,
        };
        tokio::select! {
            output = &mut invocation => return output,
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {
                let now = Instant::now();
                let past_deadline = scope.deadline.is_some_and(|deadline| now >= deadline);
                deadline_reported |= past_deadline;
                if now >= report_at {
                    next_report = next_report.saturating_mul(2);
                }
                tracing::warn!(
                    target: "bmux_server::invoke_service",
                    interface = %interface_id,
                    operation = %operation,
                    elapsed_ms = now.duration_since(started).as_millis(),
                    past_deadline,
                    waiting_on = %scope.describe_waits(),
                    "typed service invocation still running",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_lists_running_calls_in_order_and_keeps_the_slowest() {
        let scope = RequestScope::root(None);
        let outer = scope.enter_nested("a/v1.first".into());
        let inner = scope.enter_nested("b/v1.second".into());
        let waits = scope.describe_waits();
        assert!(waits.starts_with("a/v1.first "), "{waits}");
        assert!(waits.contains(" > b/v1.second "), "{waits}");
        drop(inner);
        std::thread::sleep(Duration::from_millis(2));
        drop(outer);
        assert_eq!(scope.describe_waits(), "");
        assert!(scope.describe_slowest().starts_with("a/v1.first "));
    }

    #[tokio::test]
    async fn the_current_scope_follows_run_and_defaults_to_no_deadline() {
        assert_eq!(RequestScope::current().deadline(), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        let seen = RequestScope::root(Some(deadline))
            .run(async { RequestScope::current().deadline() })
            .await;
        assert_eq!(seen, Some(deadline));
    }
}
