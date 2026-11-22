//! What simulations tell the dashboard besides simvar's own events, and what the dashboard
//! tells them back.
//!
//! A simulation emits these through simvar (`simvar::emit`); the dashboard downcasts them.
//! [`Interest`] goes the other way: which runs someone is looking at (so only those pay for
//! detailed telemetry) and which kinds of event pause them.

use std::collections::BTreeSet;
use std::sync::{PoisonError, RwLock};

use simvar_observer::RunId;

/// One invariant of a simulation: how often a run checked it, and how it failed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InvariantStatus {
    /// What it says.
    pub name: String,
    /// Checks so far.
    pub checked: u64,
    /// Each failure, as reported.
    pub failures: Vec<String>,
}

/// Telemetry a simulation emits with every invariant's status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Invariants(pub Vec<InvariantStatus>);

/// A run's heartbeat: how many scheduling decisions it made so far. Cheap enough to send for
/// every run, so the fleet shows each worker's pace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pulse {
    /// Decisions so far.
    pub decisions: u64,
}

/// Tags describing what a run exercised (`personality:windows`, `crash`, `panes:3`). The fleet
/// counts how many runs showed each, and splits its outcomes by them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Shape(pub Vec<String>);

/// How an event reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Tone {
    /// Ordinary activity.
    #[default]
    Neutral,
    /// Worth noticing.
    Info,
    /// Went well.
    Good,
    /// Something unusual.
    Warn,
    /// Something failed.
    Bad,
}

/// One thing that happened in a run, at a scheduling decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    /// The decision it happened at.
    pub decision: u64,
    /// What kind of thing (`open`, `crash`, `signal`); breakpoints and filters use it.
    pub kind: String,
    /// What exactly, for people.
    pub label: String,
    /// How it reads.
    pub tone: Tone,
    /// The entity it concerns (a [`crate::SimView`] entity id), if any.
    pub entity: Option<String>,
}

/// Buckets of decisions an index keeps at most; it halves its resolution beyond.
pub const INDEX_BUCKETS: usize = 512;
/// Events an index keeps at most (the earliest are kept).
pub const INDEX_EVENTS: usize = 4096;

/// What the timeline draws for a run, recorded as it executes: which simulated thread ran at
/// each decision (bucketed), and the run's events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIndex {
    /// Decisions recorded.
    pub decisions: u64,
    /// Decisions per bucket.
    pub bucket: u64,
    /// Thread names, by the scheduler's thread index.
    pub threads: Vec<String>,
    /// For each thread, how many decisions in each bucket picked it to run.
    pub activity: Vec<Vec<u32>>,
    /// The run's events, in order.
    pub events: Vec<TimelineEvent>,
    /// The scheduler's trace hash when last recorded.
    pub trace_hash: u64,
    /// Whether the run ended.
    pub finished: bool,
}

impl Default for RunIndex {
    fn default() -> Self {
        Self {
            decisions: 0,
            bucket: 1,
            threads: Vec::new(),
            activity: Vec::new(),
            events: Vec::new(),
            trace_hash: 0,
            finished: false,
        }
    }
}

impl RunIndex {
    /// Record decision `number`, which picked thread `thread` (named `name`, asked only the
    /// first time it is seen).
    pub fn record(&mut self, number: u64, thread: usize, name: impl FnOnce() -> String) {
        self.decisions = self.decisions.max(number + 1);
        while self.threads.len() <= thread {
            self.threads.push(String::new());
            self.activity.push(Vec::new());
        }
        if self.threads[thread].is_empty() {
            self.threads[thread] = name();
        }
        let mut bucket = usize::try_from(number / self.bucket).unwrap_or(usize::MAX);
        while bucket >= INDEX_BUCKETS {
            self.halve();
            bucket = usize::try_from(number / self.bucket).unwrap_or(usize::MAX);
        }
        let lane = &mut self.activity[thread];
        if lane.len() <= bucket {
            lane.resize(bucket + 1, 0);
        }
        lane[bucket] = lane[bucket].saturating_add(1);
    }

    /// Record an event.
    pub fn event(&mut self, event: TimelineEvent) {
        if self.events.len() < INDEX_EVENTS {
            self.events.push(event);
        }
    }

    /// Merge pairs of buckets, doubling the decisions each covers.
    fn halve(&mut self) {
        self.bucket = self.bucket.saturating_mul(2);
        for lane in &mut self.activity {
            *lane = lane
                .chunks(2)
                .map(|pair| pair.iter().fold(0_u32, |sum, n| sum.saturating_add(*n)))
                .collect();
        }
    }

    /// Buckets recorded.
    #[must_use]
    pub fn buckets(&self) -> usize {
        self.activity.iter().map(Vec::len).max().unwrap_or(0)
    }

    /// The first event at or after `decision` whose kind is `kind` (any kind for `None`).
    #[must_use]
    pub fn next_event(&self, decision: u64, kind: Option<&str>) -> Option<&TimelineEvent> {
        self.events
            .iter()
            .find(|e| e.decision > decision && kind.is_none_or(|k| e.kind == k))
    }

    /// The last event before `decision` whose kind is `kind` (any kind for `None`).
    #[must_use]
    pub fn previous_event(&self, decision: u64, kind: Option<&str>) -> Option<&TimelineEvent> {
        self.events
            .iter()
            .rev()
            .find(|e| e.decision < decision && kind.is_none_or(|k| e.kind == k))
    }
}

/// What the dashboard is interested in, shared with the simulations: which runs it watches
/// (only they send detailed telemetry) and which kinds of event pause a watched run.
#[derive(Debug, Default)]
pub struct Interest {
    watched: RwLock<BTreeSet<RunId>>,
    event_breaks: RwLock<BTreeSet<String>>,
}

impl Interest {
    /// Whether `run` is being watched.
    #[must_use]
    pub fn watching(&self, run: RunId) -> bool {
        self.watched
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&run)
    }

    /// Watch exactly `runs`.
    pub fn set_watched(&self, runs: impl IntoIterator<Item = RunId>) {
        *self.watched.write().unwrap_or_else(PoisonError::into_inner) = runs.into_iter().collect();
    }

    /// Whether a watched run pauses at events of `kind`.
    #[must_use]
    pub fn breaks_on(&self, kind: &str) -> bool {
        self.event_breaks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(kind)
    }

    /// Pause (or stop pausing) watched runs at events of `kind`.
    pub fn set_break(&self, kind: &str, on: bool) {
        let mut breaks = self
            .event_breaks
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if on {
            breaks.insert(kind.to_owned());
        } else {
            breaks.remove(kind);
        }
    }

    /// The kinds of event that pause watched runs.
    #[must_use]
    pub fn event_breaks(&self) -> Vec<String> {
        self.event_breaks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_halves_its_resolution_to_stay_bounded() {
        let mut index = RunIndex::default();
        for number in 0..(INDEX_BUCKETS as u64 * 3) {
            index.record(number, usize::try_from(number % 2).unwrap(), || {
                format!("t{}", number % 2)
            });
        }
        assert!(index.buckets() <= INDEX_BUCKETS);
        assert_eq!(index.bucket, 4);
        assert_eq!(index.threads, ["t0", "t1"]);
        let total: u64 = index.activity.iter().flatten().map(|n| u64::from(*n)).sum();
        assert_eq!(total, INDEX_BUCKETS as u64 * 3);
    }

    #[test]
    fn events_are_found_either_side_of_a_decision() {
        let mut index = RunIndex::default();
        for (decision, kind) in [(5, "open"), (9, "crash"), (12, "open")] {
            index.event(TimelineEvent {
                decision,
                kind: kind.into(),
                label: String::new(),
                tone: Tone::Neutral,
                entity: None,
            });
        }
        assert_eq!(index.next_event(5, Some("open")).unwrap().decision, 12);
        assert_eq!(index.next_event(5, None).unwrap().decision, 9);
        assert_eq!(index.previous_event(12, None).unwrap().decision, 9);
        assert!(index.previous_event(5, None).is_none());
    }

    #[test]
    fn interest_tracks_watched_runs_and_event_breaks() {
        let interest = Interest::default();
        let run = |run| RunId { worker: 1, run };
        interest.set_watched([run(3)]);
        assert!(interest.watching(run(3)));
        assert!(!interest.watching(run(4)));
        interest.set_break("crash", true);
        assert!(interest.breaks_on("crash"));
        interest.set_break("crash", false);
        assert!(!interest.breaks_on("crash"));
    }
}
