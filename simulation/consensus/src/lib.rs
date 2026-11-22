//! Model checking of bmux's owned consensus engine with `simvar::explore`.
//!
//! The model runs real [`Engine`]s. Each node has a durable store that changes only
//! when a persistence effect completes, so a crash recovers exactly what was durable.
//! Actions deliver, drop or duplicate in-flight messages, complete one node's next
//! durable operation, fire election and heartbeat timers, propose, change membership
//! and crash nodes. Ghost history records every leader and committed entry ever seen,
//! so safety is checked across the whole path rather than only the current state.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::collections::{BTreeMap, BTreeSet};

use bmux_cluster_plugin::consensus_engine::{
    Config, Effect, Engine, Input, Message, NodeId, Recovered, TimerKind,
};
use bmux_cluster_plugin::consensus_model::{
    Entry, EntryPayload, HardState, LogIdentity, LogIndex, Membership, StoredMembership, Term,
};
use simvar::explore::{Model, Properties};

/// Exploration bounds for one model configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub voters: u64,
    /// Extra nodes provisioned as learners.
    pub learners: u64,
    pub max_term: u64,
    pub proposals: u8,
    pub crashes: u8,
    pub drops: u8,
    pub duplicates: u8,
    /// Heartbeats advance per-peer sequence numbers, so they need a budget for
    /// the reachable state space to be finite.
    pub heartbeats: u8,
    pub max_messages: usize,
    /// Promote every learner to a voter once (joint consensus).
    pub membership_change: bool,
    /// Remove one voter (possibly the leader itself) through joint consensus.
    pub removal: bool,
}

impl Bounds {
    /// Small enough for exhaustive search, large enough for elections, failover,
    /// a crash with lost unsynced state, and message loss.
    #[must_use]
    pub const fn exhaustive() -> Self {
        Self {
            voters: 3,
            learners: 0,
            max_term: 2,
            proposals: 1,
            crashes: 1,
            drops: 1,
            duplicates: 0,
            heartbeats: 2,
            max_messages: 4,
            membership_change: false,
            removal: false,
        }
    }

    /// Fault-free elections and replication, small enough to finish quickly.
    #[must_use]
    pub const fn exhaustive_fault_free() -> Self {
        Self {
            voters: 3,
            learners: 0,
            max_term: 2,
            proposals: 1,
            crashes: 0,
            drops: 0,
            duplicates: 0,
            heartbeats: 1,
            max_messages: 3,
            membership_change: false,
            removal: false,
        }
    }

    /// Deeper bounds for seeded random walks.
    #[must_use]
    pub const fn walk() -> Self {
        Self {
            voters: 3,
            learners: 1,
            max_term: 6,
            proposals: 4,
            crashes: 3,
            drops: 6,
            duplicates: 3,
            heartbeats: u8::MAX,
            max_messages: 12,
            membership_change: true,
            removal: true,
        }
    }
}

pub struct Raft {
    pub bounds: Bounds,
}

/// What a node has durably written.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Store {
    hard_state: HardState,
    snapshot: Option<(LogIdentity, StoredMembership)>,
    entries: Vec<Entry>,
    commit: Option<LogIdentity>,
    applied: Option<LogIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Node {
    engine: Engine,
    store: Store,
    /// Issued durable operations, completed in order.
    pending: Vec<Effect>,
    /// Armed timers, as the driver keeps them: firing consumes the timer.
    election: Option<u64>,
    heartbeat: Option<u64>,
}

// Independent ghost-history and action-budget flags; grouping them would not
// make the model clearer.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct State {
    nodes: Vec<Node>,
    /// In-flight messages as a sorted multiset of (from, to, message).
    network: Vec<(usize, usize, Message)>,
    proposals: u8,
    crashes: u8,
    drops: u8,
    duplicates: u8,
    heartbeats: u8,
    changed: bool,
    removed: bool,
    /// Ghost history: every (term, leader) and committed entry ever observed.
    leaders: BTreeMap<Term, BTreeSet<usize>>,
    committed: BTreeMap<u64, LogIdentity>,
    conflicting_commit: bool,
    stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Deliver(usize),
    Drop(usize),
    Duplicate(usize),
    Persist(usize),
    Election(usize),
    Heartbeat(usize),
    Propose(usize),
    ChangeMembership(usize),
    RemoveVoter { leader: usize, victim: usize },
    Crash(usize),
}

fn node_id(index: usize) -> NodeId {
    let mut bytes = [0; 32];
    bytes[24..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
    NodeId::from_bytes(bytes)
}

/// A total, deterministic order for the in-flight multiset.
fn network_key(entry: &(usize, usize, Message)) -> (usize, usize, String) {
    (entry.0, entry.1, format!("{:?}", entry.2))
}

fn index_of(id: NodeId, nodes: usize) -> Option<usize> {
    (0..nodes).find(|index| node_id(*index) == id)
}

fn membership(voters: &[usize], records: &[usize]) -> Membership {
    Membership::new(
        vec![voters.iter().copied().map(node_id).collect()],
        records
            .iter()
            .map(|index| (node_id(*index), format!("node-{index}")))
            .collect(),
    )
    .expect("valid model membership")
}

impl Raft {
    fn total(&self) -> usize {
        usize::try_from(self.bounds.voters + self.bounds.learners).expect("small model")
    }

    fn voters(&self) -> Vec<usize> {
        (0..usize::try_from(self.bounds.voters).expect("small model")).collect()
    }

    fn founding(&self) -> Store {
        let all = (0..self.total()).collect::<Vec<_>>();
        let identity = LogIdentity {
            term: Term(0),
            leader: node_id(0),
            index: LogIndex(0),
        };
        Store {
            hard_state: HardState {
                term: Term(0),
                voted_for: None,
            },
            snapshot: None,
            entries: vec![Entry {
                identity,
                payload: EntryPayload::Membership(membership(&self.voters(), &all)),
            }],
            commit: Some(identity),
            applied: None,
        }
    }

    fn recover(index: usize, store: &Store) -> (Engine, Vec<Effect>) {
        Engine::recover(
            node_id(index),
            Config::default(),
            Recovered {
                hard_state: store.hard_state,
                snapshot: store.snapshot.clone(),
                entries: store.entries.clone(),
                commit: store.commit,
                applied: store.applied,
            },
        )
        .expect("a durable store always recovers")
    }

    /// Feed `input` to a node and route every effect it produces.
    fn step(state: &mut State, index: usize, input: Input) {
        let mut inputs = vec![input];
        while let Some(input) = inputs.pop() {
            let effects = state.nodes[index].engine.handle(input);
            for effect in effects {
                match effect {
                    Effect::Send { to, message } => {
                        if let Some(to) = index_of(to, state.nodes.len()) {
                            let entry = (index, to, message);
                            let key = network_key(&entry);
                            let at = state.network.partition_point(|m| network_key(m) < key);
                            state.network.insert(at, entry);
                        }
                    }
                    Effect::Persist { .. } | Effect::InstallSnapshot { .. } => {
                        state.nodes[index].pending.push(effect);
                    }
                    Effect::SetTimer {
                        kind, generation, ..
                    } => match kind {
                        TimerKind::Election => state.nodes[index].election = Some(generation),
                        TimerKind::Heartbeat => state.nodes[index].heartbeat = Some(generation),
                    },
                    Effect::Apply { through } => {
                        // Application is local and durable with its watermark.
                        state.nodes[index].store.applied = Some(through);
                        inputs.push(Input::Applied { through });
                    }
                    Effect::Stopped { .. } => state.stopped = true,
                    _ => {}
                }
            }
        }
        Self::observe(state);
    }

    /// Complete one node's next durable operation against its store.
    fn persist(state: &mut State, index: usize) {
        let node = &mut state.nodes[index];
        let effect = node.pending.remove(0);
        let id = match effect {
            Effect::Persist { id, write } => {
                let store = &mut node.store;
                if let Some(hard_state) = write.hard_state {
                    store.hard_state = hard_state;
                }
                if let Some(from) = write.truncate_from {
                    store.entries.retain(|entry| entry.identity.index < from);
                }
                store.entries.extend(write.entries);
                if let Some(commit) = write.commit {
                    store.commit = Some(commit);
                }
                id
            }
            Effect::InstallSnapshot {
                id,
                last_included,
                membership,
                ..
            } => {
                let store = &mut node.store;
                let keep = store
                    .entries
                    .iter()
                    .any(|entry| entry.identity == last_included);
                if keep {
                    store
                        .entries
                        .retain(|entry| entry.identity.index > last_included.index);
                } else {
                    store.entries.clear();
                }
                store.snapshot = Some((last_included, membership));
                if store.commit.is_none_or(|c| c.index < last_included.index) {
                    store.commit = Some(last_included);
                }
                if store.applied.is_none_or(|a| a.index < last_included.index) {
                    store.applied = Some(last_included);
                }
                id
            }
            _ => unreachable!("only durable effects are pending"),
        };
        Self::step(state, index, Input::Persisted { id });
    }

    fn observe(state: &mut State) {
        for (index, node) in state.nodes.iter().enumerate() {
            let engine = &node.engine;
            if engine.is_leader() {
                state
                    .leaders
                    .entry(engine.term())
                    .or_default()
                    .insert(index);
            }
            let Some(commit) = engine.commit() else {
                continue;
            };
            let boundary = engine.snapshot_boundary();
            for identity in engine
                .entries()
                .map(|entry| entry.identity)
                .chain(boundary)
                .filter(|identity| identity.index <= commit.index)
            {
                match state.committed.get(&identity.index.0) {
                    Some(seen) if *seen != identity => state.conflicting_commit = true,
                    Some(_) => {}
                    None => {
                        state.committed.insert(identity.index.0, identity);
                    }
                }
            }
        }
    }
}

fn election_safety(_: &Raft, state: &State) -> bool {
    state.leaders.values().all(|leaders| leaders.len() <= 1)
}

const fn state_machine_safety(_: &Raft, state: &State) -> bool {
    !state.conflicting_commit
}

/// Matching identities at an index imply identical logs up to it.
fn log_matching(_: &Raft, state: &State) -> bool {
    let logs = state
        .nodes
        .iter()
        .map(|node| {
            node.engine
                .entries()
                .map(|entry| (entry.identity.index.0, entry.identity))
                .collect::<BTreeMap<_, _>>()
        })
        .collect::<Vec<_>>();
    for (a, left) in logs.iter().enumerate() {
        for right in &logs[a + 1..] {
            // Walking down from the top, once two logs agree at an index they
            // must agree at every lower index both retain.
            let mut agreed = false;
            for (index, identity) in left.iter().rev() {
                match right.get(index) {
                    Some(other) if other == identity => agreed = true,
                    Some(_) if agreed => return false,
                    _ => {}
                }
            }
        }
    }
    true
}

/// A leader's log contains every entry committed in an earlier term.
fn leader_completeness(_: &Raft, state: &State) -> bool {
    state.nodes.iter().all(|node| {
        let engine = &node.engine;
        if !engine.is_leader() {
            return true;
        }
        let first = engine
            .snapshot_boundary()
            .map_or(0, |boundary| boundary.index.0 + 1);
        state
            .committed
            .iter()
            .filter(|(index, identity)| **index >= first && identity.term < engine.term())
            .all(|(index, identity)| {
                engine.entry(LogIndex(*index)).map(|entry| entry.identity) == Some(*identity)
            })
    })
}

fn commit_never_exceeds_log(_: &Raft, state: &State) -> bool {
    state.nodes.iter().all(|node| {
        let engine = &node.engine;
        engine.applied().map(|a| a.index) <= engine.commit().map(|c| c.index)
            && engine.commit().map(|c| c.index) <= engine.last_log().map(|l| l.index)
    })
}

/// An initialized voter that is not leading always has an election deadline, or
/// a lost leader could never be replaced.
fn election_timer_armed(_: &Raft, state: &State) -> bool {
    state.nodes.iter().all(|node| {
        let engine = &node.engine;
        let voter = engine.effective_membership().is_some_and(|stored| {
            stored
                .membership
                .configurations()
                .iter()
                .any(|voters| voters.contains(&engine.id()))
        });
        engine.is_leader() || !voter || node.election.is_some()
    })
}

const fn never_stopped(_: &Raft, state: &State) -> bool {
    !state.stopped
}

fn proposal_committed(_: &Raft, state: &State) -> bool {
    state.nodes.iter().any(|node| {
        node.engine
            .commit()
            .and_then(|commit| node.engine.entry(commit.index))
            .is_some_and(|entry| matches!(entry.payload, EntryPayload::Application(_)))
    })
}

fn membership_changed(_: &Raft, state: &State) -> bool {
    state.nodes.iter().any(|node| {
        node.engine.committed_membership().is_some_and(|stored| {
            stored.membership.configurations().len() == 1
                && stored.membership.configurations()[0].len() > 3
        })
    })
}

impl Model for Raft {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<State> {
        let store = self.founding();
        let mut state = State {
            nodes: Vec::new(),
            network: Vec::new(),
            proposals: 0,
            crashes: 0,
            drops: 0,
            duplicates: 0,
            heartbeats: 0,
            changed: false,
            removed: false,
            leaders: BTreeMap::new(),
            committed: BTreeMap::new(),
            conflicting_commit: false,
            stopped: false,
        };
        for index in 0..self.total() {
            let (engine, _) = Self::recover(index, &store);
            state.nodes.push(Node {
                engine,
                store: store.clone(),
                pending: Vec::new(),
                election: Some(1),
                heartbeat: None,
            });
        }
        Self::observe(&mut state);
        vec![state]
    }

    fn actions(&self, state: &State, actions: &mut Vec<Action>) {
        if state.stopped {
            return;
        }
        let mut previous = None;
        for (at, message) in state.network.iter().enumerate() {
            // Identical in-flight messages are interchangeable.
            if previous == Some(message) {
                continue;
            }
            previous = Some(message);
            actions.push(Action::Deliver(at));
            if state.drops < self.bounds.drops {
                actions.push(Action::Drop(at));
            }
            if state.duplicates < self.bounds.duplicates {
                actions.push(Action::Duplicate(at));
            }
        }
        for (index, node) in state.nodes.iter().enumerate() {
            if !node.pending.is_empty() {
                actions.push(Action::Persist(index));
            }
            if node.engine.is_leader() {
                if state.heartbeats < self.bounds.heartbeats && node.heartbeat.is_some() {
                    actions.push(Action::Heartbeat(index));
                }
                if state.proposals < self.bounds.proposals {
                    actions.push(Action::Propose(index));
                }
                if self.bounds.membership_change && !state.changed {
                    actions.push(Action::ChangeMembership(index));
                }
                if self.bounds.removal && !state.removed {
                    for victim in self.voters() {
                        actions.push(Action::RemoveVoter {
                            leader: index,
                            victim,
                        });
                    }
                }
            }
            // Leaders ignore a stale election timer, but it is still consumed.
            if node.election.is_some()
                && (node.engine.is_leader() || node.engine.term().0 < self.bounds.max_term)
            {
                actions.push(Action::Election(index));
            }
            if state.crashes < self.bounds.crashes {
                actions.push(Action::Crash(index));
            }
        }
    }

    fn next_state(&self, state: &State, action: &Action) -> Option<State> {
        let mut next = state.clone();
        match *action {
            Action::Deliver(at) => {
                let (from, to, message) = next.network.remove(at);
                Self::step(
                    &mut next,
                    to,
                    Input::Message {
                        from: node_id(from),
                        message,
                    },
                );
            }
            Action::Drop(at) => {
                next.network.remove(at);
                next.drops += 1;
            }
            Action::Duplicate(at) => {
                let copy = next.network[at].clone();
                next.network.insert(at, copy);
                next.duplicates += 1;
            }
            Action::Persist(index) => Self::persist(&mut next, index),
            Action::Election(index) => {
                let generation = next.nodes[index].election.take()?;
                Self::step(
                    &mut next,
                    index,
                    Input::Timer {
                        kind: TimerKind::Election,
                        generation,
                    },
                );
            }
            Action::Heartbeat(index) => {
                next.heartbeats += 1;
                let generation = next.nodes[index].heartbeat.take()?;
                Self::step(
                    &mut next,
                    index,
                    Input::Timer {
                        kind: TimerKind::Heartbeat,
                        generation,
                    },
                );
            }
            Action::Propose(index) => {
                next.proposals += 1;
                let value = next.proposals;
                Self::step(
                    &mut next,
                    index,
                    Input::Propose {
                        token: u64::from(value),
                        payload: vec![value],
                    },
                );
            }
            Action::ChangeMembership(index) => {
                next.changed = true;
                let all = (0..self.total()).collect::<Vec<_>>();
                Self::step(
                    &mut next,
                    index,
                    Input::ChangeMembership {
                        token: 1_000,
                        target: membership(&all, &all),
                    },
                );
            }
            Action::RemoveVoter { leader, victim } => {
                next.removed = true;
                let all = (0..self.total()).collect::<Vec<_>>();
                let remaining = self
                    .voters()
                    .into_iter()
                    .filter(|voter| *voter != victim)
                    .collect::<Vec<_>>();
                Self::step(
                    &mut next,
                    leader,
                    Input::ChangeMembership {
                        token: 2_000,
                        target: membership(&remaining, &all),
                    },
                );
            }
            Action::Crash(index) => {
                next.crashes += 1;
                let node = &mut next.nodes[index];
                let (engine, _) = Self::recover(index, &node.store);
                node.engine = engine;
                node.pending.clear();
                node.election = Some(1);
                node.heartbeat = None;
                Self::observe(&mut next);
            }
        }
        Some(next)
    }

    fn properties(&self, properties: &mut Properties<Self>) {
        properties
            .always("election safety", election_safety)
            .always("state machine safety", state_machine_safety)
            .always("log matching", log_matching)
            .always("leader completeness", leader_completeness)
            .always("commit and apply within the log", commit_never_exceeds_log)
            .always("no node stops", never_stopped)
            .always(
                "non-leader voters keep an election deadline",
                election_timer_armed,
            )
            .sometimes("a proposal commits", proposal_committed);
        if self.bounds.membership_change {
            properties.sometimes("a learner is promoted", membership_changed);
        }
    }

    fn within_boundary(&self, state: &State) -> bool {
        state.network.len() <= self.bounds.max_messages
            && state
                .nodes
                .iter()
                .all(|node| node.engine.term().0 <= self.bounds.max_term)
    }
}
