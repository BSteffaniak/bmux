//! Pure, sans-IO owned Raft engine (ADR-0015).
//!
//! [`Engine::handle`] consumes one input and returns the effects it requires.
//! It performs no I/O, reads no clock and draws no randomness: adapters execute
//! effects in order, report durable completion, and feed timers and
//! authenticated peer messages back in. Replies that depend on durable state are
//! released only after the covering [`Input::Persisted`], and a leader counts
//! itself toward a quorum only through entries it has durably persisted.
//! Receiver-side append and vote safety reuse the model's evaluators.

use crate::consensus_model::{
    AppendOutcome, AppendReconciliation, AppendRequest, Entry, EntryPayload, HardState,
    IdentityExhaustion, LogIdentity, LogIndex, Membership, ReconcileError, RequestVote,
    StoredMembership, Term,
};
pub use crate::membership::NodeId;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

#[cfg(test)]
mod tests;

/// Local scheduling and batching bounds. These are not wire contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Config {
    /// The driver draws each election deadline uniformly from this range.
    pub election_timeout: (Duration, Duration),
    pub heartbeat_interval: Duration,
    pub max_append_entries: usize,
    pub max_append_bytes: usize,
    /// Pipelined append batches per peer once its log is known to match.
    pub max_inflight: usize,
    /// A learner must be this close to the leader's log before promotion.
    pub learner_catchup_lag: u64,
    /// A leader refuses proposals while this many entries are uncommitted, so a
    /// leader without quorum pushes back instead of growing its log until the
    /// store's retention limit stops the node.
    pub max_uncommitted: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            election_timeout: (Duration::from_millis(450), Duration::from_millis(900)),
            heartbeat_interval: Duration::from_millis(75),
            max_append_entries: 64,
            max_append_bytes: 1_048_576,
            max_inflight: 4,
            learner_catchup_lag: 64,
            max_uncommitted: 1_024,
        }
    }
}

/// Engine-level peer messages. Adapters map these onto the negotiated generated
/// contracts and bind the sender to the authenticated peer before delivery.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Message {
    Vote {
        term: Term,
        last_log: Option<LogIdentity>,
    },
    VoteReply {
        term: Term,
        granted: bool,
    },
    Append {
        term: Term,
        seq: u64,
        previous: Option<LogIdentity>,
        entries: Vec<Entry>,
        leader_commit: Option<LogIdentity>,
    },
    AppendReply {
        term: Term,
        seq: u64,
        outcome: AppendReplyOutcome,
    },
    /// The snapshot bytes travel through staged transfer; this carries its boundary.
    Snapshot {
        term: Term,
        seq: u64,
        last_included: LogIdentity,
        membership: StoredMembership,
    },
    SnapshotReply {
        term: Term,
        seq: u64,
        installed: Option<LogIdentity>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppendReplyOutcome {
    /// Durable through this identity. `None` means an empty log matched.
    Matched(Option<LogIdentity>),
    /// A retry hint, never evidence that any prefix committed.
    Conflict { next: LogIndex },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimerKind {
    Election,
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Input {
    /// An authenticated peer message; `from` is the authenticated sender.
    Message {
        from: NodeId,
        message: Message,
    },
    Timer {
        kind: TimerKind,
        generation: u64,
    },
    Propose {
        token: u64,
        payload: Vec<u8>,
    },
    /// Change to a uniform target configuration through joint consensus.
    ChangeMembership {
        token: u64,
        target: Membership,
    },
    Read {
        token: u64,
    },
    /// Every durable operation with an id at or below this one completed.
    Persisted {
        id: u64,
    },
    PersistFailed {
        id: u64,
    },
    /// Durable application reached this identity.
    Applied {
        through: LogIdentity,
    },
    /// The driver durably compacted through an applied, snapshotted boundary.
    Compacted {
        through: LogIdentity,
        membership: StoredMembership,
    },
    /// Sending a snapshot to a peer failed; it will be retried.
    SnapshotSendFailed {
        peer: NodeId,
        seq: u64,
    },
}

/// One durable log transaction: hard state, suffix replacement and commit
/// watermark are written together, in issue order, before completion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Write {
    pub hard_state: Option<HardState>,
    /// Delete retained entries at or after this index before appending.
    pub truncate_from: Option<LogIndex>,
    pub entries: Vec<Entry>,
    pub commit: Option<LogIdentity>,
}

impl Write {
    const fn is_empty(&self) -> bool {
        self.hard_state.is_none()
            && self.truncate_from.is_none()
            && self.entries.is_empty()
            && self.commit.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Execute durably and in id order, then report [`Input::Persisted`].
    Persist {
        id: u64,
        write: Write,
    },
    /// Install the staged snapshot durably (ordered with persists), then report it.
    InstallSnapshot {
        id: u64,
        from: NodeId,
        last_included: LogIdentity,
        membership: StoredMembership,
    },
    Send {
        to: NodeId,
        message: Message,
    },
    /// Transfer the latest snapshot to `to`; its reply arrives as a message.
    SendSnapshot {
        to: NodeId,
        term: Term,
        seq: u64,
    },
    /// Fire [`Input::Timer`] after a delay drawn from `[min, max]`.
    SetTimer {
        kind: TimerKind,
        generation: u64,
        min: Duration,
        max: Duration,
    },
    /// Apply committed entries through this durable commit identity.
    Apply {
        through: LogIdentity,
    },
    Proposed {
        token: u64,
        identity: LogIdentity,
    },
    Rejected {
        token: u64,
        reason: Rejection,
    },
    MembershipProposed {
        token: u64,
        identity: LogIdentity,
    },
    ReadReady {
        token: u64,
        index: LogIdentity,
    },
    ReadFailed {
        token: u64,
    },
    /// An observation, never permission to acknowledge uncommitted work.
    Leadership {
        term: Term,
        leader: Option<NodeId>,
    },
    /// A peer sent a malformed or impossible message; it was ignored.
    ProtocolViolation {
        from: NodeId,
    },
    /// Durable authority is uncertain or corrupt; the engine accepts no input.
    Stopped {
        reason: StopReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rejection {
    NotLeader {
        hint: Option<NodeId>,
    },
    /// Too many entries are uncommitted; retry once the quorum catches up.
    Backpressure,
    MembershipChangeInProgress,
    InvalidMembership,
    LearnerNotCaughtUp(NodeId),
    LeaderNotReady,
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopReason {
    PersistFailed,
    Corruption(ReconcileError),
    Exhausted(IdentityExhaustion),
}

/// Durable state recovered by the storage adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub hard_state: HardState,
    pub snapshot: Option<(LogIdentity, StoredMembership)>,
    pub entries: Vec<Entry>,
    pub commit: Option<LogIdentity>,
    pub applied: Option<LogIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryError {
    NonContiguousLog,
    InvalidPositions,
}

/// Retained entries after an optional compacted boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Log {
    snapshot: Option<LogIdentity>,
    snapshot_membership: Option<StoredMembership>,
    entries: VecDeque<Entry>,
}

impl Log {
    fn first_index(&self) -> LogIndex {
        self.snapshot
            .map_or(LogIndex(0), |boundary| LogIndex(boundary.index.0 + 1))
    }

    fn last(&self) -> Option<LogIdentity> {
        self.entries
            .back()
            .map(|entry| entry.identity)
            .or(self.snapshot)
    }

    fn next_index(&self) -> Result<LogIndex, IdentityExhaustion> {
        self.last()
            .map_or(Ok(LogIndex(0)), |last| last.index.successor())
    }

    fn get(&self, index: LogIndex) -> Option<&Entry> {
        let offset = index.0.checked_sub(self.first_index().0)?;
        self.entries.get(usize::try_from(offset).ok()?)
    }

    fn identity_at(&self, index: LogIndex) -> Option<LogIdentity> {
        match self.snapshot {
            Some(boundary) if boundary.index == index => Some(boundary),
            _ => self.get(index).map(|entry| entry.identity),
        }
    }

    fn contains(&self, identity: Option<LogIdentity>) -> bool {
        identity.is_none_or(|identity| self.identity_at(identity.index) == Some(identity))
    }

    /// Retained and matching, or inside the compacted (committed) prefix.
    fn covers(&self, identity: Option<LogIdentity>) -> bool {
        self.contains(identity)
            || identity.is_some_and(|identity| {
                self.snapshot
                    .is_some_and(|boundary| identity.index < boundary.index)
            })
    }

    fn truncate_from(&mut self, index: LogIndex) {
        let Some(offset) = index.0.checked_sub(self.first_index().0) else {
            self.entries.clear();
            return;
        };
        self.entries
            .truncate(usize::try_from(offset).unwrap_or(usize::MAX));
    }

    fn entries_from(&self, index: LogIndex, max_entries: usize, max_bytes: usize) -> Vec<Entry> {
        let mut batch = Vec::new();
        let mut bytes = 0usize;
        let mut next = index;
        while batch.len() < max_entries {
            let Some(entry) = self.get(next) else { break };
            let size = payload_bytes(&entry.payload);
            if !batch.is_empty() && bytes.saturating_add(size) > max_bytes {
                break;
            }
            bytes = bytes.saturating_add(size);
            batch.push(entry.clone());
            next = LogIndex(next.0 + 1);
        }
        batch
    }

    /// Latest membership at or below `limit` (all retained entries when `None`).
    fn membership_through(&self, limit: Option<LogIndex>) -> Option<StoredMembership> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| limit.is_none_or(|limit| entry.identity.index <= limit))
            .find_map(|entry| match &entry.payload {
                EntryPayload::Membership(membership) => Some(StoredMembership {
                    introduced_at: Some(entry.identity),
                    membership: membership.clone(),
                }),
                _ => None,
            })
            .or_else(|| self.snapshot_membership.clone())
    }

    /// Drop entries through a boundary, keeping a matching suffix.
    fn compact(&mut self, through: LogIdentity, membership: StoredMembership) {
        if self.identity_at(through.index) == Some(through) {
            while self
                .entries
                .front()
                .is_some_and(|entry| entry.identity.index <= through.index)
            {
                self.entries.pop_front();
            }
        } else {
            self.entries.clear();
        }
        self.snapshot = Some(through);
        self.snapshot_membership = Some(membership);
    }
}

fn payload_bytes(payload: &EntryPayload) -> usize {
    match payload {
        EntryPayload::NoOp => 0,
        EntryPayload::Application(bytes) => bytes.len(),
        EntryPayload::Membership(membership) => membership
            .nodes()
            .values()
            .map(|endpoint| endpoint.len() + 40)
            .sum::<usize>()
            .saturating_add(64),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Peer {
    matched: Option<LogIdentity>,
    next: LogIndex,
    /// Sequence numbers of appends awaiting a reply.
    inflight: VecDeque<u64>,
    /// After a conflict or restart only one probe is outstanding.
    probing: bool,
    snapshot: Option<u64>,
    /// Highest sequence this peer answered in the current term.
    acked: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadRound {
    token: u64,
    index: LogIdentity,
    /// Replies to appends sent at or after this sequence confirm leadership.
    after: u64,
    confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Leader {
    peers: BTreeMap<NodeId, Peer>,
    reads: Vec<ReadRound>,
    /// Membership change requested by a client, finished by the uniform entry.
    transition: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Role {
    Follower { leader: Option<NodeId> },
    Candidate { granted: BTreeSet<NodeId> },
    Leader(Leader),
}

/// Work released only when a durable operation completes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Deferred {
    Send(NodeId, Message),
    SelfVote(Term),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Durable {
    Write {
        last: Option<LogIdentity>,
        hard_state: HardState,
        commit: Option<LogIdentity>,
    },
    Snapshot {
        last_included: LogIdentity,
        membership: StoredMembership,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Engine {
    id: NodeId,
    config: Config,
    stopped: Option<StopReason>,
    hard_state: HardState,
    log: Log,
    commit: Option<LogIdentity>,
    applied: Option<LogIdentity>,
    apply_requested: Option<LogIdentity>,
    durable_last: Option<LogIdentity>,
    durable_commit: Option<LogIdentity>,
    role: Role,
    next_durable: u64,
    pending: BTreeMap<u64, (Durable, Vec<Deferred>)>,
    timers: (u64, u64),
    seq: u64,
    /// Accumulated write for the input being handled.
    write: Write,
}

impl Engine {
    /// Start from recovered durable authority as a follower.
    /// # Errors
    /// Rejects incoherent recovered positions instead of inferring authority.
    pub fn recover(
        id: NodeId,
        config: Config,
        recovered: Recovered,
    ) -> Result<(Self, Vec<Effect>), RecoveryError> {
        let (snapshot, snapshot_membership) = recovered
            .snapshot
            .map_or((None, None), |(at, m)| (Some(at), Some(m)));
        let log = Log {
            snapshot,
            snapshot_membership,
            entries: recovered.entries.into(),
        };
        let mut expected = log.first_index();
        let mut previous = log.snapshot;
        for entry in &log.entries {
            if entry.identity.index != expected
                || previous.is_some_and(|previous| previous.term > entry.identity.term)
            {
                return Err(RecoveryError::NonContiguousLog);
            }
            previous = Some(entry.identity);
            expected = LogIndex(expected.0 + 1);
        }
        if !log.contains(recovered.commit)
            || !log.contains(recovered.applied)
            || recovered.applied.map(|a| a.index) > recovered.commit.map(|c| c.index)
            || recovered.commit.map(|c| c.index) < log.snapshot.map(|s| s.index)
        {
            return Err(RecoveryError::InvalidPositions);
        }
        let last = log.last();
        let mut engine = Self {
            id,
            config,
            stopped: None,
            hard_state: recovered.hard_state,
            log,
            commit: recovered.commit,
            applied: recovered.applied,
            apply_requested: recovered.applied,
            durable_last: last,
            durable_commit: recovered.commit,
            role: Role::Follower { leader: None },
            next_durable: 1,
            pending: BTreeMap::new(),
            timers: (0, 0),
            seq: 0,
            write: Write::default(),
        };
        let mut effects = Vec::new();
        engine.reset_election_timer(&mut effects);
        engine.request_apply(&mut effects);
        Ok((engine, effects))
    }

    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    #[must_use]
    pub const fn term(&self) -> Term {
        self.hard_state.term
    }

    #[must_use]
    pub const fn hard_state(&self) -> HardState {
        self.hard_state
    }

    #[must_use]
    pub const fn is_leader(&self) -> bool {
        matches!(self.role, Role::Leader(_))
    }

    #[must_use]
    pub const fn leader_hint(&self) -> Option<NodeId> {
        match &self.role {
            Role::Follower { leader } => *leader,
            Role::Leader(_) => Some(self.id),
            Role::Candidate { .. } => None,
        }
    }

    #[must_use]
    pub const fn commit(&self) -> Option<LogIdentity> {
        self.commit
    }

    #[must_use]
    pub const fn applied(&self) -> Option<LogIdentity> {
        self.applied
    }

    #[must_use]
    pub fn last_log(&self) -> Option<LogIdentity> {
        self.log.last()
    }

    #[must_use]
    pub fn entry(&self, index: LogIndex) -> Option<&Entry> {
        self.log.get(index)
    }

    /// Retained entries after the compacted boundary, in index order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.log.entries.iter()
    }

    #[must_use]
    pub const fn snapshot_boundary(&self) -> Option<LogIdentity> {
        self.log.snapshot
    }

    #[must_use]
    pub fn effective_membership(&self) -> Option<StoredMembership> {
        self.log.membership_through(None)
    }

    #[must_use]
    pub fn committed_membership(&self) -> Option<StoredMembership> {
        self.log.membership_through(Some(self.commit?.index))
    }

    #[must_use]
    pub const fn stopped(&self) -> Option<StopReason> {
        self.stopped
    }

    /// Advance the engine by one input.
    pub fn handle(&mut self, input: Input) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(reason) = self.stopped {
            effects.push(Effect::Stopped { reason });
            return effects;
        }
        let result = match input {
            Input::Message { from, message } => self.on_message(from, message, &mut effects),
            Input::Timer { kind, generation } => self.on_timer(kind, generation, &mut effects),
            Input::Propose { token, payload } => {
                self.propose(token, EntryPayload::Application(payload), &mut effects)
            }
            Input::ChangeMembership { token, target } => {
                self.change_membership(token, target, &mut effects)
            }
            Input::Read { token } => {
                self.read(token, &mut effects);
                Ok(())
            }
            Input::Persisted { id } => self.on_persisted(id, &mut effects),
            Input::PersistFailed { .. } => Err(StopReason::PersistFailed),
            Input::Applied { through } => {
                self.on_applied(through, &mut effects);
                Ok(())
            }
            Input::Compacted {
                through,
                membership,
            } => {
                if self
                    .applied
                    .is_some_and(|applied| through.index <= applied.index)
                    && self.log.identity_at(through.index) == Some(through)
                {
                    self.log.compact(through, membership);
                }
                Ok(())
            }
            Input::SnapshotSendFailed { peer, seq } => {
                if let Role::Leader(leader) = &mut self.role
                    && let Some(state) = leader.peers.get_mut(&peer)
                    && state.snapshot == Some(seq)
                {
                    state.snapshot = None;
                }
                Ok(())
            }
        };
        if let Err(reason) = result {
            self.stop(reason, &mut effects);
            return effects;
        }
        self.flush_write(Vec::new(), &mut effects);
        effects
    }

    fn stop(&mut self, reason: StopReason, effects: &mut Vec<Effect>) {
        self.stopped = Some(reason);
        self.write = Write::default();
        self.pending.clear();
        if let Role::Leader(leader) = &mut self.role {
            for read in leader.reads.drain(..) {
                effects.push(Effect::ReadFailed { token: read.token });
            }
        }
        effects.push(Effect::Stopped { reason });
    }

    // ----- durability -------------------------------------------------------

    /// Release `deferred` after all durable work issued so far, including the
    /// write accumulated by this input; immediately when nothing is pending.
    fn after_durable(&mut self, deferred: Vec<Deferred>, effects: &mut Vec<Effect>) {
        if !self.write.is_empty() {
            self.flush_write(deferred, effects);
        } else if let Some((_, (_, waiting))) = self.pending.iter_mut().next_back() {
            waiting.extend(deferred);
        } else {
            for item in deferred {
                self.release(item, effects);
            }
        }
    }

    fn flush_write(&mut self, deferred: Vec<Deferred>, effects: &mut Vec<Effect>) {
        if self.write.is_empty() {
            if !deferred.is_empty() {
                self.after_durable(deferred, effects);
            }
            return;
        }
        let write = std::mem::take(&mut self.write);
        let id = self.next_durable;
        self.next_durable += 1;
        self.pending.insert(
            id,
            (
                Durable::Write {
                    last: self.log.last(),
                    hard_state: self.hard_state,
                    commit: self.commit,
                },
                deferred,
            ),
        );
        effects.push(Effect::Persist { id, write });
    }

    fn set_hard_state(&mut self, hard_state: HardState) {
        if self.hard_state != hard_state {
            self.hard_state = hard_state;
            self.write.hard_state = Some(hard_state);
        }
    }

    fn truncate_log(&mut self, from: LogIndex) {
        self.log.truncate_from(from);
        self.write
            .entries
            .retain(|entry| entry.identity.index < from);
        self.write.truncate_from = Some(
            self.write
                .truncate_from
                .map_or(from, |existing| existing.min(from)),
        );
        if self.durable_last.is_some_and(|last| last.index >= from) {
            self.durable_last = self.log.last();
        }
    }

    fn append_entry(&mut self, entry: Entry) {
        self.write.entries.push(entry.clone());
        self.log.entries.push_back(entry);
    }

    fn set_commit(&mut self, commit: LogIdentity) {
        if self
            .commit
            .is_none_or(|current| commit.index > current.index)
        {
            self.commit = Some(commit);
            self.write.commit = Some(commit);
        }
    }

    fn on_persisted(&mut self, id: u64, effects: &mut Vec<Effect>) -> Result<(), StopReason> {
        let completed = self
            .pending
            .range(..=id)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for done in completed {
            let Some((durable, deferred)) = self.pending.remove(&done) else {
                continue;
            };
            match durable {
                Durable::Write { last, commit, .. } => {
                    if self.log.contains(last) {
                        self.durable_last = last;
                    }
                    if commit.map(|c| c.index) > self.durable_commit.map(|c| c.index)
                        && self.log.contains(commit)
                    {
                        self.durable_commit = commit;
                    }
                }
                Durable::Snapshot {
                    last_included,
                    membership,
                } => {
                    self.log.compact(last_included, membership);
                    self.durable_last = self.log.last();
                    if self
                        .commit
                        .is_none_or(|commit| commit.index < last_included.index)
                    {
                        self.commit = Some(last_included);
                    }
                    if self
                        .durable_commit
                        .is_none_or(|commit| commit.index < last_included.index)
                    {
                        self.durable_commit = Some(last_included);
                    }
                    if self
                        .applied
                        .is_none_or(|applied| applied.index < last_included.index)
                    {
                        self.applied = Some(last_included);
                        self.apply_requested = Some(last_included);
                    }
                }
            }
            for item in deferred {
                self.release(item, effects);
            }
        }
        if self.is_leader() {
            self.advance_commit(effects)?;
        }
        self.request_apply(effects);
        Ok(())
    }

    fn release(&mut self, item: Deferred, effects: &mut Vec<Effect>) {
        match item {
            Deferred::Send(to, message) => effects.push(Effect::Send { to, message }),
            Deferred::SelfVote(term) => {
                if term == self.hard_state.term
                    && let Role::Candidate { granted } = &mut self.role
                {
                    granted.insert(self.id);
                    if let Err(reason) = self.check_election(effects) {
                        self.stop(reason, effects);
                    }
                }
            }
        }
    }

    fn request_apply(&mut self, effects: &mut Vec<Effect>) {
        if let Some(commit) = self.durable_commit
            && self
                .apply_requested
                .is_none_or(|requested| commit.index > requested.index)
        {
            self.apply_requested = Some(commit);
            effects.push(Effect::Apply { through: commit });
        }
    }

    fn on_applied(&mut self, through: LogIdentity, effects: &mut Vec<Effect>) {
        if self
            .applied
            .is_none_or(|applied| through.index > applied.index)
            && self
                .durable_commit
                .is_some_and(|commit| through.index <= commit.index)
        {
            self.applied = Some(through);
            self.release_reads(effects);
        }
    }

    // ----- timers -------------------------------------------------------------

    fn reset_election_timer(&mut self, effects: &mut Vec<Effect>) {
        self.timers.0 += 1;
        effects.push(Effect::SetTimer {
            kind: TimerKind::Election,
            generation: self.timers.0,
            min: self.config.election_timeout.0,
            max: self.config.election_timeout.1,
        });
    }

    fn reset_heartbeat_timer(&mut self, effects: &mut Vec<Effect>) {
        self.timers.1 += 1;
        effects.push(Effect::SetTimer {
            kind: TimerKind::Heartbeat,
            generation: self.timers.1,
            min: self.config.heartbeat_interval,
            max: self.config.heartbeat_interval,
        });
    }

    fn on_timer(
        &mut self,
        kind: TimerKind,
        generation: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        match kind {
            TimerKind::Election if generation == self.timers.0 && !self.is_leader() => {
                self.campaign(effects)
            }
            TimerKind::Heartbeat if generation == self.timers.1 && self.is_leader() => {
                self.broadcast(true, effects);
                self.reset_heartbeat_timer(effects);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    // ----- elections ----------------------------------------------------------

    fn is_voter(membership: Option<&StoredMembership>, id: NodeId) -> bool {
        membership.is_some_and(|stored| {
            stored
                .membership
                .configurations()
                .iter()
                .any(|set| set.contains(&id))
        })
    }

    fn campaign(&mut self, effects: &mut Vec<Effect>) -> Result<(), StopReason> {
        let membership = self.effective_membership();
        if !Self::is_voter(membership.as_ref(), self.id) {
            // Learners and uninitialized nodes wait for a leader.
            self.reset_election_timer(effects);
            return Ok(());
        }
        let term = self
            .hard_state
            .term
            .successor()
            .map_err(StopReason::Exhausted)?;
        self.set_hard_state(HardState {
            term,
            voted_for: Some(self.id),
        });
        self.role = Role::Candidate {
            granted: BTreeSet::new(),
        };
        effects.push(Effect::Leadership { term, leader: None });
        self.reset_election_timer(effects);
        let last_log = self.log.last();
        let mut deferred = vec![Deferred::SelfVote(term)];
        for peer in Self::voters(membership.as_ref()) {
            if peer != self.id {
                deferred.push(Deferred::Send(peer, Message::Vote { term, last_log }));
            }
        }
        self.after_durable(deferred, effects);
        Ok(())
    }

    fn voters(membership: Option<&StoredMembership>) -> BTreeSet<NodeId> {
        membership
            .map(|stored| {
                stored
                    .membership
                    .configurations()
                    .iter()
                    .flatten()
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn check_election(&mut self, effects: &mut Vec<Effect>) -> Result<(), StopReason> {
        let Role::Candidate { granted } = &self.role else {
            return Ok(());
        };
        let Some(membership) = self.effective_membership() else {
            return Ok(());
        };
        if membership.membership.has_quorum(granted) {
            self.become_leader(effects)?;
        }
        Ok(())
    }

    fn become_leader(&mut self, effects: &mut Vec<Effect>) -> Result<(), StopReason> {
        let term = self.hard_state.term;
        let next = self.log.next_index().map_err(StopReason::Exhausted)?;
        let mut leader = Leader {
            peers: BTreeMap::new(),
            reads: Vec::new(),
            transition: None,
        };
        if let Some(membership) = self.effective_membership() {
            for peer in membership.membership.nodes().keys() {
                if *peer != self.id {
                    leader.peers.insert(*peer, Self::new_peer(next));
                }
            }
        }
        self.role = Role::Leader(leader);
        effects.push(Effect::Leadership {
            term,
            leader: Some(self.id),
        });
        // The current-term no-op commits earlier terms indirectly and gates reads.
        self.append_entry(Entry {
            identity: LogIdentity {
                term,
                leader: self.id,
                index: next,
            },
            payload: EntryPayload::NoOp,
        });
        self.broadcast(true, effects);
        self.reset_heartbeat_timer(effects);
        self.advance_commit(effects)
    }

    const fn new_peer(next: LogIndex) -> Peer {
        Peer {
            matched: None,
            next,
            inflight: VecDeque::new(),
            probing: true,
            snapshot: None,
            acked: 0,
        }
    }

    fn step_down(&mut self, term: Term, leader: Option<NodeId>, effects: &mut Vec<Effect>) {
        if term > self.hard_state.term {
            self.set_hard_state(HardState {
                term,
                voted_for: None,
            });
        }
        let changed = match &self.role {
            Role::Follower { leader: current } => *current != leader,
            _ => true,
        };
        let was_leader = matches!(self.role, Role::Leader(_));
        if let Role::Leader(state) = &mut self.role {
            for read in state.reads.drain(..) {
                effects.push(Effect::ReadFailed { token: read.token });
            }
        }
        self.role = Role::Follower { leader };
        if was_leader {
            // A leader keeps no election deadline; without a fresh one a deposed
            // leader with the freshest log could never campaign again.
            self.reset_election_timer(effects);
        }
        if changed {
            effects.push(Effect::Leadership {
                term: self.hard_state.term,
                leader,
            });
        }
    }

    fn on_vote(
        &mut self,
        from: NodeId,
        term: Term,
        last_log: Option<LogIdentity>,
        effects: &mut Vec<Effect>,
    ) {
        if term > self.hard_state.term {
            self.step_down(term, None, effects);
        }
        let membership = self.effective_membership();
        let response = match membership {
            Some(stored) => {
                let decision = self.hard_state.evaluate_vote(
                    self.id,
                    self.log.last(),
                    &stored.membership,
                    RequestVote {
                        term,
                        candidate: from,
                        last_log,
                    },
                );
                if let Some(next) = decision.persist {
                    self.set_hard_state(next);
                }
                if decision.response.granted {
                    // Granting resets the deadline so a viable candidate can finish.
                    self.reset_election_timer(effects);
                }
                decision.response
            }
            // Uninitialized membership observes the term but never votes.
            None => crate::consensus_model::VoteResponse {
                term: self.hard_state.term,
                granted: false,
            },
        };
        self.after_durable(
            vec![Deferred::Send(
                from,
                Message::VoteReply {
                    term: response.term,
                    granted: response.granted,
                },
            )],
            effects,
        );
    }

    fn on_vote_reply(
        &mut self,
        from: NodeId,
        term: Term,
        granted: bool,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        if term > self.hard_state.term {
            self.step_down(term, None, effects);
            self.reset_election_timer(effects);
            return Ok(());
        }
        if term < self.hard_state.term || !granted {
            return Ok(());
        }
        if let Role::Candidate { granted } = &mut self.role {
            granted.insert(from);
        }
        // The self-vote joins only after it is durable, so a quorum of remote
        // grants alone cannot elect a candidate whose own vote could be lost.
        if let Role::Candidate { granted } = &self.role
            && granted.contains(&self.id)
        {
            self.check_election(effects)?;
        }
        Ok(())
    }

    // ----- replication (follower) --------------------------------------------

    fn on_append(
        &mut self,
        from: NodeId,
        request: AppendRequest,
        seq: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        if self.is_leader() && request.term == self.hard_state.term {
            // Election safety allows one leader per term.
            effects.push(Effect::ProtocolViolation { from });
            return Ok(());
        }
        let snapshot = self.log.snapshot;
        let decision = {
            let log = &self.log;
            self.hard_state.evaluate_append(
                &request,
                (self.commit, snapshot),
                |index| Ok(log.get(index)),
                (self.config.max_append_entries, self.config.max_append_bytes),
            )
        };
        let reply = |term, outcome| Message::AppendReply { term, seq, outcome };
        let outcome = match decision.outcome {
            Ok(AppendOutcome::StaleTerm) => {
                effects.push(Effect::Send {
                    to: from,
                    message: reply(
                        self.hard_state.term,
                        AppendReplyOutcome::Conflict {
                            next: self.log.first_index(),
                        },
                    ),
                });
                return Ok(());
            }
            Ok(AppendOutcome::Reconciled(outcome)) => outcome,
            Err(ReconcileError::CompactedPrefix) => {
                // Our compacted prefix is committed; the leader resumes after it.
                self.accept_leader(from, request.term, effects);
                let next = self.log.first_index();
                self.after_durable(
                    vec![Deferred::Send(
                        from,
                        reply(request.term, AppendReplyOutcome::Conflict { next }),
                    )],
                    effects,
                );
                return Ok(());
            }
            Err(ReconcileError::InvalidRequest(_)) => {
                effects.push(Effect::ProtocolViolation { from });
                return Ok(());
            }
            Err(error) => return Err(StopReason::Corruption(error)),
        };
        if let Some(next) = decision.persist {
            self.set_hard_state(next);
        }
        self.accept_leader(from, request.term, effects);
        match outcome {
            AppendReconciliation::PreviousMismatch => {
                let next = self.conflict_hint(request.previous);
                self.after_durable(
                    vec![Deferred::Send(
                        from,
                        reply(request.term, AppendReplyOutcome::Conflict { next }),
                    )],
                    effects,
                );
            }
            AppendReconciliation::Matched {
                through,
                replace_from,
            } => {
                if let Some(offset) = replace_from {
                    let first = request.entries[offset].identity.index;
                    if self.log.get(first).is_some() {
                        self.truncate_log(first);
                    }
                    for entry in request.entries.into_iter().skip(offset) {
                        self.append_entry(entry);
                    }
                }
                if let Some(commit) = decision.commit {
                    self.set_commit(commit);
                }
                self.after_durable(
                    vec![Deferred::Send(
                        from,
                        reply(request.term, AppendReplyOutcome::Matched(through)),
                    )],
                    effects,
                );
            }
        }
        Ok(())
    }

    fn accept_leader(&mut self, from: NodeId, term: Term, effects: &mut Vec<Effect>) {
        if !matches!(&self.role, Role::Follower { leader: Some(current) } if *current == from)
            || term > self.hard_state.term
        {
            self.step_down(term, Some(from), effects);
        }
        self.reset_election_timer(effects);
    }

    /// First index of the conflicting term, never below the committed prefix.
    fn conflict_hint(&self, previous: Option<LogIdentity>) -> LogIndex {
        let first = self.log.first_index();
        let floor = self
            .commit
            .map_or(first, |commit| LogIndex(commit.index.0 + 1))
            .max(first);
        let Some(previous) = previous else {
            return floor;
        };
        let Some(local) = self.log.identity_at(previous.index) else {
            return self
                .log
                .next_index()
                .unwrap_or(previous.index)
                .min(previous.index)
                .max(floor);
        };
        let mut index = local.index;
        while index > floor
            && let Some(prior) = index.0.checked_sub(1).map(LogIndex)
            && self
                .log
                .identity_at(prior)
                .is_some_and(|identity| identity.term == local.term)
        {
            index = prior;
        }
        index.max(floor)
    }

    fn on_snapshot(
        &mut self,
        from: NodeId,
        term: Term,
        seq: u64,
        last_included: LogIdentity,
        membership: StoredMembership,
        effects: &mut Vec<Effect>,
    ) {
        if term < self.hard_state.term {
            effects.push(Effect::Send {
                to: from,
                message: Message::SnapshotReply {
                    term: self.hard_state.term,
                    seq,
                    installed: None,
                },
            });
            return;
        }
        self.accept_leader(from, term, effects);
        let reply = Message::SnapshotReply {
            term,
            seq,
            installed: Some(last_included),
        };
        if self
            .commit
            .is_some_and(|commit| commit.index >= last_included.index)
        {
            self.after_durable(vec![Deferred::Send(from, reply)], effects);
            return;
        }
        // Order the install after writes already issued, then reply.
        self.flush_write(Vec::new(), effects);
        let id = self.next_durable;
        self.next_durable += 1;
        self.pending.insert(
            id,
            (
                Durable::Snapshot {
                    last_included,
                    membership: membership.clone(),
                },
                vec![Deferred::Send(from, reply)],
            ),
        );
        effects.push(Effect::InstallSnapshot {
            id,
            from,
            last_included,
            membership,
        });
    }

    // ----- replication (leader) ----------------------------------------------

    fn broadcast(&mut self, heartbeat: bool, effects: &mut Vec<Effect>) {
        let peers = match &self.role {
            Role::Leader(leader) => leader.peers.keys().copied().collect::<Vec<_>>(),
            _ => return,
        };
        for peer in peers {
            self.replicate(peer, heartbeat, effects);
        }
    }

    /// Send what the peer's window allows. A heartbeat always produces one
    /// message, anchored at the peer's known match when its window is full, so
    /// slow persistence or a saturated pipeline never suppresses leadership.
    fn replicate(&mut self, peer: NodeId, heartbeat: bool, effects: &mut Vec<Effect>) {
        let term = self.hard_state.term;
        let commit = self.commit;
        let config = self.config;
        let seq = self.seq + 1;
        let Role::Leader(leader) = &mut self.role else {
            return;
        };
        let Some(state) = leader.peers.get_mut(&peer) else {
            return;
        };
        if state.snapshot.is_some() {
            return;
        }
        if heartbeat && state.probing {
            // A lost probe or reply must not stall the peer: retransmit it.
            state.inflight.clear();
        }
        let window = if state.probing {
            1
        } else {
            config.max_inflight
        };
        let mut sent = false;
        while state.inflight.len() < window {
            // Entries before the first retained index exist only in the snapshot.
            if state.next < self.log.first_index() {
                state.snapshot = Some(seq);
                self.seq = seq;
                effects.push(Effect::SendSnapshot {
                    to: peer,
                    term,
                    seq,
                });
                return;
            }
            let previous = state
                .next
                .0
                .checked_sub(1)
                .and_then(|index| self.log.identity_at(LogIndex(index)));
            let entries = self.log.entries_from(
                state.next,
                config.max_append_entries,
                config.max_append_bytes,
            );
            if entries.is_empty() && (!heartbeat || sent) {
                break;
            }
            let seq = self.seq + 1;
            self.seq = seq;
            state.inflight.push_back(seq);
            if !state.probing
                && let Some(last) = entries.last()
            {
                state.next = LogIndex(last.identity.index.0 + 1);
            }
            sent = true;
            effects.push(Effect::Send {
                to: peer,
                message: Message::Append {
                    term,
                    seq,
                    previous,
                    entries,
                    leader_commit: commit,
                },
            });
            if state.probing {
                break;
            }
        }
        if heartbeat && !sent {
            // Window full: an empty append at the known match still proves
            // leadership and carries the commit watermark.
            let seq = self.seq + 1;
            self.seq = seq;
            effects.push(Effect::Send {
                to: peer,
                message: Message::Append {
                    term,
                    seq,
                    previous: state.matched,
                    entries: Vec::new(),
                    leader_commit: commit,
                },
            });
        }
    }

    fn on_append_reply(
        &mut self,
        from: NodeId,
        term: Term,
        seq: u64,
        outcome: AppendReplyOutcome,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        if term > self.hard_state.term {
            self.step_down(term, None, effects);
            self.reset_election_timer(effects);
            return Ok(());
        }
        if term < self.hard_state.term {
            return Ok(());
        }
        let log_next = self.log.next_index().map_err(StopReason::Exhausted)?;
        let Role::Leader(leader) = &mut self.role else {
            return Ok(());
        };
        let Some(state) = leader.peers.get_mut(&from) else {
            return Ok(());
        };
        state.acked = state.acked.max(seq);
        let was_inflight = state.inflight.contains(&seq);
        state.inflight.retain(|pending| *pending > seq);
        match outcome {
            AppendReplyOutcome::Matched(through) => {
                if !self.log.covers(through) {
                    effects.push(Effect::ProtocolViolation { from });
                    return Ok(());
                }
                if through.map(|t| t.index) > state.matched.map(|m| m.index) {
                    state.matched = through;
                }
                let floor = state
                    .matched
                    .map_or(LogIndex(0), |m| LogIndex(m.index.0 + 1));
                // With nothing outstanding, entries sent past the match were lost.
                if state.next < floor || state.probing || state.inflight.is_empty() {
                    state.next = floor;
                }
                state.probing = false;
            }
            AppendReplyOutcome::Conflict { next } => {
                if was_inflight {
                    let floor = state
                        .matched
                        .map_or(LogIndex(0), |matched| LogIndex(matched.index.0 + 1));
                    state.next = next.max(floor).min(log_next);
                    state.probing = true;
                    state.inflight.clear();
                }
            }
        }
        self.advance_commit(effects)?;
        self.confirm_reads(effects);
        self.replicate(from, false, effects);
        Ok(())
    }

    fn on_snapshot_reply(
        &mut self,
        from: NodeId,
        term: Term,
        seq: u64,
        installed: Option<LogIdentity>,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        if term > self.hard_state.term {
            self.step_down(term, None, effects);
            self.reset_election_timer(effects);
            return Ok(());
        }
        let Role::Leader(leader) = &mut self.role else {
            return Ok(());
        };
        let Some(state) = leader.peers.get_mut(&from) else {
            return Ok(());
        };
        if term < self.hard_state.term || state.snapshot != Some(seq) {
            return Ok(());
        }
        state.snapshot = None;
        state.acked = state.acked.max(seq);
        if let Some(installed) = installed
            && installed.index.0 + 1 > state.next.0
        {
            if installed.index.0 >= state.matched.map_or(0, |m| m.index.0) {
                state.matched = Some(installed);
            }
            state.next = LogIndex(installed.index.0 + 1);
            state.probing = false;
        }
        self.advance_commit(effects)?;
        self.confirm_reads(effects);
        self.replicate(from, false, effects);
        Ok(())
    }

    /// Commit the highest current-term index matched durably by every quorum.
    fn advance_commit(&mut self, effects: &mut Vec<Effect>) -> Result<(), StopReason> {
        let Role::Leader(leader) = &self.role else {
            return Ok(());
        };
        let Some(membership) = self.effective_membership() else {
            return Ok(());
        };
        let mut matches = leader
            .peers
            .iter()
            .filter_map(|(peer, state)| state.matched.map(|m| (*peer, m.index)))
            .collect::<Vec<_>>();
        if let Some(own) = self.durable_last
            && self.log.contains(Some(own))
        {
            matches.push((self.id, own.index));
        }
        let mut candidates = matches.iter().map(|(_, index)| *index).collect::<Vec<_>>();
        candidates.sort_unstable();
        candidates.dedup();
        let current = self.commit.map(|commit| commit.index);
        for index in candidates.into_iter().rev() {
            if current.is_some_and(|current| index <= current) {
                break;
            }
            let Some(identity) = self.log.identity_at(index) else {
                continue;
            };
            if identity.term != self.hard_state.term {
                // Earlier terms commit only indirectly through a current-term entry.
                continue;
            }
            let acknowledged = matches
                .iter()
                .filter(|(_, matched)| *matched >= index)
                .map(|(peer, _)| *peer)
                .collect::<BTreeSet<_>>();
            if membership.membership.has_quorum(&acknowledged) {
                self.set_commit(identity);
                self.on_commit_advanced(effects)?;
                break;
            }
        }
        Ok(())
    }

    /// Finish joint transitions and retire a leader removed by the final entry.
    fn on_commit_advanced(&mut self, effects: &mut Vec<Effect>) -> Result<(), StopReason> {
        let (Some(committed), Some(effective)) =
            (self.committed_membership(), self.effective_membership())
        else {
            return Ok(());
        };
        if committed != effective {
            return Ok(());
        }
        let configurations = committed.membership.configurations();
        if configurations.len() == 2 {
            // Removed voters leave the records; demoting to learner is a later change.
            let removed = configurations[0]
                .difference(&configurations[1])
                .copied()
                .collect::<BTreeSet<_>>();
            let target = Membership::new(
                vec![configurations[1].iter().copied().collect()],
                committed
                    .membership
                    .nodes()
                    .iter()
                    .filter(|(id, _)| !removed.contains(id))
                    .map(|(id, endpoint)| (*id, endpoint.clone()))
                    .collect(),
            )
            .map_err(|_| StopReason::Corruption(ReconcileError::InvalidRecoveryPositions))?;
            self.append_leader_entry(EntryPayload::Membership(target), effects)?;
        } else if !configurations[0].contains(&self.id) {
            // Publish the final commit before retiring so the new configuration
            // can elect without first rediscovering it.
            self.broadcast(true, effects);
            let term = self.hard_state.term;
            self.step_down(term, None, effects);
            self.reset_election_timer(effects);
        } else if let Role::Leader(leader) = &mut self.role {
            leader.transition = None;
            self.sync_peers();
        }
        Ok(())
    }

    fn sync_peers(&mut self) {
        let Some(membership) = self.effective_membership() else {
            return;
        };
        let next = self.log.next_index().unwrap_or(LogIndex(u64::MAX));
        let id = self.id;
        if let Role::Leader(leader) = &mut self.role {
            let nodes = membership.membership.nodes();
            leader.peers.retain(|peer, _| nodes.contains_key(peer));
            for peer in nodes.keys() {
                if *peer != id {
                    leader
                        .peers
                        .entry(*peer)
                        .or_insert_with(|| Self::new_peer(next));
                }
            }
        }
    }

    fn append_leader_entry(
        &mut self,
        payload: EntryPayload,
        effects: &mut Vec<Effect>,
    ) -> Result<LogIdentity, StopReason> {
        let index = self.log.next_index().map_err(StopReason::Exhausted)?;
        let identity = LogIdentity {
            term: self.hard_state.term,
            leader: self.id,
            index,
        };
        let membership_change = matches!(payload, EntryPayload::Membership(_));
        self.append_entry(Entry { identity, payload });
        if membership_change {
            // The new configuration is effective as soon as it is appended.
            self.sync_peers();
            self.invalidate_reads(effects);
        }
        self.broadcast(false, effects);
        Ok(identity)
    }

    // ----- client operations -----------------------------------------------

    fn propose(
        &mut self,
        token: u64,
        payload: EntryPayload,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        if !self.is_leader() {
            effects.push(Effect::Rejected {
                token,
                reason: Rejection::NotLeader {
                    hint: self.leader_hint(),
                },
            });
            return Ok(());
        }
        let uncommitted = self
            .log
            .last()
            .map_or(0, |last| last.index.0)
            .saturating_sub(self.commit.map_or(0, |commit| commit.index.0));
        if uncommitted >= self.config.max_uncommitted {
            effects.push(Effect::Rejected {
                token,
                reason: Rejection::Backpressure,
            });
            return Ok(());
        }
        let identity = self.append_leader_entry(payload, effects)?;
        effects.push(Effect::Proposed { token, identity });
        self.advance_commit(effects)
    }

    fn change_membership(
        &mut self,
        token: u64,
        target: Membership,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        let reject = |reason| Effect::Rejected { token, reason };
        let Role::Leader(leader) = &self.role else {
            effects.push(reject(Rejection::NotLeader {
                hint: self.leader_hint(),
            }));
            return Ok(());
        };
        let (Some(committed), Some(effective)) =
            (self.committed_membership(), self.effective_membership())
        else {
            effects.push(reject(Rejection::LeaderNotReady));
            return Ok(());
        };
        if committed != effective
            || leader.transition.is_some()
            || effective.membership.configurations().len() != 1
        {
            effects.push(reject(Rejection::MembershipChangeInProgress));
            return Ok(());
        }
        if target.configurations().len() != 1 {
            effects.push(reject(Rejection::InvalidMembership));
            return Ok(());
        }
        let current = &effective.membership.configurations()[0];
        let wanted = &target.configurations()[0];
        let last = self.log.last().map_or(0, |last| last.index.0);
        for added in wanted.difference(current) {
            let caught_up = effective.membership.nodes().contains_key(added)
                && leader.peers.get(added).is_some_and(|peer| {
                    peer.matched.is_some_and(|matched| {
                        last.saturating_sub(matched.index.0) <= self.config.learner_catchup_lag
                    })
                });
            if !caught_up {
                effects.push(reject(Rejection::LearnerNotCaughtUp(*added)));
                return Ok(());
            }
        }
        let payload = if wanted == current {
            // Learner records only: voting authority is unchanged.
            target
        } else {
            let mut records = effective.membership.nodes().clone();
            records.extend(
                target
                    .nodes()
                    .iter()
                    .map(|(id, endpoint)| (*id, endpoint.clone())),
            );
            let Ok(joint) = Membership::new(
                vec![
                    current.iter().copied().collect(),
                    wanted.iter().copied().collect(),
                ],
                records.into_iter().collect(),
            ) else {
                effects.push(reject(Rejection::InvalidMembership));
                return Ok(());
            };
            joint
        };
        let identity = self.append_leader_entry(EntryPayload::Membership(payload), effects)?;
        if let Role::Leader(leader) = &mut self.role {
            leader.transition = Some(token);
        }
        effects.push(Effect::MembershipProposed { token, identity });
        self.advance_commit(effects)
    }

    fn read(&mut self, token: u64, effects: &mut Vec<Effect>) {
        let term = self.hard_state.term;
        let ready = self
            .commit
            .is_some_and(|commit| commit.term == term && self.is_leader());
        if !self.is_leader() {
            effects.push(Effect::Rejected {
                token,
                reason: Rejection::NotLeader {
                    hint: self.leader_hint(),
                },
            });
            return;
        }
        if !ready {
            effects.push(Effect::Rejected {
                token,
                reason: Rejection::LeaderNotReady,
            });
            return;
        }
        let index = self.commit.unwrap_or(LogIdentity {
            term,
            leader: self.id,
            index: LogIndex(0),
        });
        let after = self.seq + 1;
        if let Role::Leader(leader) = &mut self.role {
            leader.reads.push(ReadRound {
                token,
                index,
                after,
                confirmed: false,
            });
        }
        self.broadcast(true, effects);
        self.confirm_reads(effects);
    }

    fn confirm_reads(&mut self, effects: &mut Vec<Effect>) {
        let Some(membership) = self.effective_membership() else {
            return;
        };
        let id = self.id;
        if let Role::Leader(leader) = &mut self.role {
            for read in &mut leader.reads {
                if read.confirmed {
                    continue;
                }
                let mut acknowledged = leader
                    .peers
                    .iter()
                    .filter(|(_, peer)| peer.acked >= read.after)
                    .map(|(peer, _)| *peer)
                    .collect::<BTreeSet<_>>();
                acknowledged.insert(id);
                read.confirmed = membership.membership.has_quorum(&acknowledged);
            }
        }
        self.release_reads(effects);
    }

    fn release_reads(&mut self, effects: &mut Vec<Effect>) {
        let applied = self.applied.map(|applied| applied.index);
        if let Role::Leader(leader) = &mut self.role {
            leader.reads.retain(|read| {
                let ready = read.confirmed && applied.is_some_and(|a| a >= read.index.index);
                if ready {
                    effects.push(Effect::ReadReady {
                        token: read.token,
                        index: read.index,
                    });
                }
                !ready
            });
        }
    }

    /// Confirmations from another configuration cannot be combined.
    fn invalidate_reads(&mut self, effects: &mut Vec<Effect>) {
        if let Role::Leader(leader) = &mut self.role {
            for read in leader.reads.drain(..) {
                effects.push(Effect::ReadFailed { token: read.token });
            }
        }
    }

    // ----- dispatch -----------------------------------------------------------

    fn on_message(
        &mut self,
        from: NodeId,
        message: Message,
        effects: &mut Vec<Effect>,
    ) -> Result<(), StopReason> {
        if from == self.id {
            effects.push(Effect::ProtocolViolation { from });
            return Ok(());
        }
        match message {
            Message::Vote { term, last_log } => {
                self.on_vote(from, term, last_log, effects);
                Ok(())
            }
            Message::VoteReply { term, granted } => {
                self.on_vote_reply(from, term, granted, effects)
            }
            Message::Append {
                term,
                seq,
                previous,
                entries,
                leader_commit,
            } => self.on_append(
                from,
                AppendRequest {
                    term,
                    leader: from,
                    previous,
                    entries,
                    leader_commit,
                },
                seq,
                effects,
            ),
            Message::AppendReply { term, seq, outcome } => {
                self.on_append_reply(from, term, seq, outcome, effects)
            }
            Message::Snapshot {
                term,
                seq,
                last_included,
                membership,
            } => {
                self.on_snapshot(from, term, seq, last_included, membership, effects);
                Ok(())
            }
            Message::SnapshotReply {
                term,
                seq,
                installed,
            } => self.on_snapshot_reply(from, term, seq, installed, effects),
        }
    }
}
