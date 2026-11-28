//! What the workload knows about the run: its panes, the processes it started outside the
//! runtime, what is under way, and what it did.

use std::{collections::BTreeMap, sync::Arc};

use bmux_pane_runtime_plugin::simulation::{Execution, Pane, Panes};

use super::Step;

/// What a run did, recorded in its trace (and printed with `PANE_RUNTIME_SIM_STATS`).
#[derive(Debug, Default)]
pub struct Stats {
    pub(super) opened: u64,
    pub(super) typed: u64,
    pub(super) resized: u64,
    pub(super) closed: u64,
    pub(super) crashed: u64,
    pub(super) restarted: u64,
    pub(super) cancelled: u64,
    pub(super) failed_opens: u64,
}

impl Stats {
    pub(super) fn record(&self) {
        for value in [
            self.opened,
            self.typed,
            self.resized,
            self.closed,
            self.crashed,
            self.restarted,
            self.cancelled,
            self.failed_opens,
        ] {
            simvar::record(value);
        }
        if crate::host_set("PANE_RUNTIME_SIM_STATS") {
            eprintln!("pane-runtime stats: {self:?}");
        }
    }
}

/// What the workload knows about one open pane.
#[derive(Debug)]
pub(super) struct Opened {
    /// Its program's id and serial (ids are reused; serials are not).
    pub(super) pid: u32,
    pub(super) serial: u64,
    /// The terminal its first program started on (by its serial, while it ran), if known.
    pub(super) terminal: Option<String>,
    /// Lines typed into its current program.
    pub(super) typed: Vec<String>,
    /// Programs it has started (one, plus one per restart begun).
    pub(super) programs: u64,
    /// Whether a restart is under way.
    pub(super) restarting: bool,
    /// Bumped by each resize, so a resize's check knows when a later one superseded it.
    pub(super) resizes: u64,
    /// Bumped by whatever may signal its program (an interrupt, a restart), so a check of
    /// what another action did to it knows when one interfered.
    pub(super) touched: u64,
}

/// A process started outside the pane runtime, after a crash.
pub(super) struct Witness {
    pub(super) serial: u64,
    /// Its terminal's name.
    pub(super) terminal: String,
    pub(super) _child: switchy_pty::PtyChild,
    pub(super) _pair: switchy_pty::PtyPair,
}

/// The workload's state, shared by its drivers.
pub(super) struct State {
    pub(super) panes: Arc<Panes>,
    pub(super) open: BTreeMap<Pane, Opened>,
    /// Opens under way, against the pane cap.
    pub(super) opening: usize,
    /// Actions left to take, across drivers.
    pub(super) steps_left: u64,
    /// Actions under way that a crash cancels.
    pub(super) busy: u64,
    /// An exclusive action is under way (see
    /// [`Kind::exclusive`](crate::profile::Kind::exclusive)): drivers start nothing
    /// until it is done.
    pub(super) paused: bool,
    /// Exits the runtime acted on, per pane.
    pub(super) exits: BTreeMap<Pane, u64>,
    pub(super) violations: Vec<String>,
    pub(super) stats: Stats,
    pub(super) witnesses: Vec<Witness>,
    /// Panes in the order they opened (splits included), for scripts.
    pub(super) order: Vec<Pane>,
    /// Each scripted driver's steps left; `None` when drivers draw their actions.
    pub(super) scripts: Option<Vec<std::collections::VecDeque<Step>>>,
    /// The step each scripted driver is taking.
    pub(super) targets: BTreeMap<u64, Step>,
    /// Program executions launched, by their mutation id.
    pub(super) executions: BTreeMap<u128, Launch>,
    /// Mutation ids handed out so far.
    pub(super) mutations: u64,
    /// The panes the last snapshot recorded, once one was taken.
    pub(super) persisted: Option<Vec<Pane>>,
}

/// A program execution the workload launched (see `executions.rs`).
pub(super) struct Launch {
    /// Its program's exit code.
    pub(super) code: u8,
    /// The execution its reservation names, once launched.
    pub(super) execution: Option<Execution>,
    /// The server generation that first launched it.
    pub(super) generation: u64,
    /// Whether it was finished (or found orphaned by a crash).
    pub(super) settled: bool,
}

impl Stats {
    /// What the run did, coarsely (each count bucketed by powers of two), for guided runs to
    /// tell new behaviour from more of the same.
    #[must_use]
    pub fn outcome(&self) -> Vec<String> {
        let bucket = |count: u64| match count {
            0 => 0,
            count => 1 + count.ilog2(),
        };
        [
            ("opened", self.opened),
            ("typed", self.typed),
            ("resized", self.resized),
            ("closed", self.closed),
            ("crashed", self.crashed),
            ("restarted", self.restarted),
            ("cancelled", self.cancelled),
            ("failed_opens", self.failed_opens),
        ]
        .into_iter()
        .map(|(name, count)| format!("{name}:{}", bucket(count)))
        .collect()
    }

    /// The counts a metamorphic run compares across personalities, exactly. Not lines typed:
    /// a pane takes input for as long as it serves its terminal, which outlasts its program
    /// longer on Windows (whose output ends only with the console), though what is typed then
    /// reaches nothing on any system.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "opened={} resized={} closed={} crashed={} restarted={} cancelled={} failed_opens={}",
            self.opened,
            self.resized,
            self.closed,
            self.crashed,
            self.restarted,
            self.cancelled,
            self.failed_opens
        )
    }

    /// Shape tags for what the run did.
    #[must_use]
    pub fn tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        if self.crashed > 0 {
            tags.push("crash".into());
        }
        if self.restarted > 0 {
            tags.push("restart".into());
        }
        if self.cancelled > 0 {
            tags.push("cancelled".into());
        }
        if self.failed_opens > 0 {
            tags.push("failed open".into());
        }
        if self.opened >= 3 {
            tags.push("3+ panes".into());
        }
        tags
    }
}
