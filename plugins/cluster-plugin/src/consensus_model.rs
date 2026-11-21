//! Owned consensus identity. Runtime adapters must not use leader identity to
//! order candidate log freshness. Index zero remains a valid durable position.
use crate::membership::NodeId;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// Validated voting configuration. An uninitialized installation is represented
/// outside this type, not as an empty configuration with a vacuous quorum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    voters: Vec<BTreeSet<NodeId>>,
    nodes: BTreeMap<NodeId, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipError {
    ConfigurationCount,
    EmptyVoters,
    DuplicateVoter,
    DuplicateNode,
    MissingNode,
}

impl Membership {
    // Fixed-width identities and count/length words are charged explicitly;
    // endpoint lengths are UTF-8 byte lengths, not character counts.
    fn admission_bytes(&self) -> Option<usize> {
        let mut bytes = 16usize;
        for configuration in &self.voters {
            bytes = bytes
                .checked_add(8)?
                .checked_add(configuration.len().checked_mul(32)?)?;
        }
        for endpoint in self.nodes.values() {
            bytes = bytes.checked_add(40)?.checked_add(endpoint.len())?;
        }
        Some(bytes)
    }

    /// Validate before canonicalizing so duplicate wire identities cannot vanish.
    /// # Errors
    /// Rejects malformed uniform/joint configurations or missing node records.
    pub fn new(
        configurations: Vec<Vec<NodeId>>,
        records: Vec<(NodeId, String)>,
    ) -> Result<Self, MembershipError> {
        if !(1..=2).contains(&configurations.len()) {
            return Err(MembershipError::ConfigurationCount);
        }
        let mut nodes = BTreeMap::new();
        for (id, endpoint) in records {
            if nodes.insert(id, endpoint).is_some() {
                return Err(MembershipError::DuplicateNode);
            }
        }
        let mut voters = Vec::new();
        for configuration in configurations {
            if configuration.is_empty() {
                return Err(MembershipError::EmptyVoters);
            }
            let mut set = BTreeSet::new();
            for id in configuration {
                if !set.insert(id) {
                    return Err(MembershipError::DuplicateVoter);
                }
                if !nodes.contains_key(&id) {
                    return Err(MembershipError::MissingNode);
                }
            }
            voters.push(set);
        }
        Ok(Self { voters, nodes })
    }

    /// A joint quorum requires an independent majority in each voter set.
    /// Learners and unknown identities do not contribute.
    #[must_use]
    pub fn has_quorum(&self, acknowledged: &BTreeSet<NodeId>) -> bool {
        self.voters
            .iter()
            .all(|set| set.intersection(acknowledged).count() > set.len() / 2)
    }

    #[must_use]
    pub fn configurations(&self) -> &[BTreeSet<NodeId>] {
        &self.voters
    }

    #[must_use]
    pub const fn nodes(&self) -> &BTreeMap<NodeId, String> {
        &self.nodes
    }
}

/// Inconsistent positions require explicit recovery, never inferred authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionError {
    MissingBoundary,
    BeyondBoundary,
    ConflictingIdentity,
    DecreasingTerm,
}

/// Validate a known prefix position against its covering boundary.
/// # Errors
/// Rejects missing coverage, index/term inversion and conflicting equal indexes.
pub fn validate_position_prefix(
    prefix: Option<LogIdentity>,
    boundary: Option<LogIdentity>,
) -> Result<(), PositionError> {
    let Some(prefix) = prefix else {
        return Ok(());
    };
    let boundary = boundary.ok_or(PositionError::MissingBoundary)?;
    if prefix.index > boundary.index {
        return Err(PositionError::BeyondBoundary);
    }
    if prefix.index == boundary.index && prefix != boundary {
        return Err(PositionError::ConflictingIdentity);
    }
    if prefix.term > boundary.term {
        return Err(PositionError::DecreasingTerm);
    }
    Ok(())
}

/// A stored configuration and its introducing position.
///
/// Absence of the whole record denotes an uninitialized installation, not a voting configuration.
/// A missing position is retained for legacy bootstrap representations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMembership {
    pub introduced_at: Option<LogIdentity>,
    pub membership: Membership,
}

/// Rejected membership-log transition or stale proposal precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipTransitionError {
    MissingPosition,
    NonAdvancingPosition,
    ConflictingIdentity,
    InvalidPosition,
    InvalidGenesis,
    DirectVoterChange,
    JointPredecessorMismatch,
    NestedJoint,
    FinalVotersMismatch,
    PredecessorNotCommitted,
    StalePrecondition,
    PendingMembership,
}

/// Validate the explicit first membership; snapshots are not genesis entries.
///
/// Standalone primitive: append, recovery, and proposal owners do not yet call
/// these transition checks. This does not enforce authorization or catch-up.
/// # Errors
/// Rejects unpositioned, nonzero-index, or joint genesis configurations.
pub fn validate_membership_genesis(
    genesis: &StoredMembership,
) -> Result<(), MembershipTransitionError> {
    let position = genesis
        .introduced_at
        .ok_or(MembershipTransitionError::MissingPosition)?;
    if position.index != LogIndex(0) || genesis.membership.configurations().len() != 1 {
        return Err(MembershipTransitionError::InvalidGenesis);
    }
    Ok(())
}

/// Validate one strictly advancing membership-log entry.
///
/// `confirmed` must be a commit watermark verified against the same accepted log
/// prefix by the adapter, not an unchecked peer claim. A follower may use the
/// incoming leader's verified commit coverage, including for a predecessor in
/// the same received batch. Recovery supplies its durable commit watermark.
/// Authorization, learner catch-up, log contiguity, and durable publication are
/// separate owner obligations. This primitive is not wired into those owners.
/// # Errors
/// Rejects illegal voter changes, ambiguous identities, and unconfirmed phases.
pub fn validate_membership_successor(
    previous: &StoredMembership,
    next: &StoredMembership,
    confirmed: Option<LogIdentity>,
) -> Result<(), MembershipTransitionError> {
    let before = previous
        .introduced_at
        .ok_or(MembershipTransitionError::MissingPosition)?;
    let after = next
        .introduced_at
        .ok_or(MembershipTransitionError::MissingPosition)?;
    if before == after && previous.membership != next.membership {
        return Err(MembershipTransitionError::ConflictingIdentity);
    }
    if after.index <= before.index {
        return Err(MembershipTransitionError::NonAdvancingPosition);
    }
    validate_position_prefix(Some(before), Some(after))
        .map_err(|_| MembershipTransitionError::InvalidPosition)?;
    if before.term == after.term && before.leader != after.leader {
        return Err(MembershipTransitionError::ConflictingIdentity);
    }
    let old = previous.membership.configurations();
    let new = next.membership.configurations();
    match (old.len(), new.len()) {
        (1, 1) if old[0] != new[0] => {
            return Err(MembershipTransitionError::DirectVoterChange);
        }
        (1, 2) if old[0] != new[0] => {
            return Err(MembershipTransitionError::JointPredecessorMismatch);
        }
        (2, 2) => return Err(MembershipTransitionError::NestedJoint),
        (2, 1) if old[1] != new[0] => {
            return Err(MembershipTransitionError::FinalVotersMismatch);
        }
        _ => {}
    }
    // Uniform node/learner updates preserve voters. Starting a joint phase or
    // finalizing one must not overlap an uncommitted membership phase.
    if (old.len() == 2 || new.len() == 2)
        && validate_position_prefix(Some(before), confirmed).is_err()
    {
        return Err(MembershipTransitionError::PredecessorNotCommitted);
    }
    Ok(())
}

/// Validate membership records in an accepted append suffix or recovered log.
///
/// `previous` is the configuration at the surviving prefix/snapshot, NOT the
/// configuration in a suffix being replaced. Skip already matched membership
/// records; exact duplicates are not advancing successors. The caller bounds
/// iteration and verifies commit coverage against its reconciled log. Ordinary
/// application entries need not be materialized here.
/// # Errors
/// Rejects illegal successors or commit identities inconsistent with known
/// membership positions. Genesis must be admitted separately.
pub fn validate_membership_sequence<'a>(
    previous: &'a StoredMembership,
    successors: impl IntoIterator<Item = &'a StoredMembership>,
    confirmed: Option<LogIdentity>,
) -> Result<(), MembershipTransitionError> {
    let mut previous = previous;
    for (offset, next) in std::iter::once(previous).chain(successors).enumerate() {
        let position = next
            .introduced_at
            .ok_or(MembershipTransitionError::MissingPosition)?;
        if let Some(confirmed) = confirmed {
            let (prefix, boundary) = if position.index <= confirmed.index {
                (position, confirmed)
            } else {
                (confirmed, position)
            };
            validate_position_prefix(Some(prefix), Some(boundary))
                .map_err(|_| MembershipTransitionError::InvalidPosition)?;
            if position.term == confirmed.term && position.leader != confirmed.leader {
                return Err(MembershipTransitionError::ConflictingIdentity);
            }
        }
        if offset != 0 {
            validate_membership_successor(previous, next, confirmed)?;
        }
        previous = next;
    }
    Ok(())
}

/// Plan one membership proposal against exact authoritative preconditions.
///
/// All membership proposals are serialized behind the preceding membership's
/// commit. Full records (position AND content), not voter-set equality, prevent
/// ABA. The owner still checks term/leadership, authorization, catch-up, and
/// atomically compares these preconditions when persisting the returned record.
/// This standalone planner does not activate production enforcement.
/// # Errors
/// Rejects stale expectations, uncommitted preceding membership, or an illegal
/// successor. Uninitialized state must use explicit genesis admission instead.
pub fn plan_membership_successor(
    effective: &StoredMembership,
    committed: &StoredMembership,
    expected_effective: &StoredMembership,
    expected_committed: &StoredMembership,
    proposed: Membership,
    introduced_at: LogIdentity,
) -> Result<StoredMembership, MembershipTransitionError> {
    if effective != expected_effective || committed != expected_committed {
        return Err(MembershipTransitionError::StalePrecondition);
    }
    let effective_position = effective
        .introduced_at
        .ok_or(MembershipTransitionError::MissingPosition)?;
    let committed_position = committed
        .introduced_at
        .ok_or(MembershipTransitionError::MissingPosition)?;
    if effective_position == committed_position && effective != committed {
        return Err(MembershipTransitionError::ConflictingIdentity);
    }
    validate_position_prefix(Some(committed_position), Some(effective_position))
        .map_err(|_| MembershipTransitionError::InvalidPosition)?;
    if effective != committed {
        return Err(MembershipTransitionError::PendingMembership);
    }
    let next = StoredMembership {
        introduced_at: Some(introduced_at),
        membership: proposed,
    };
    validate_membership_successor(effective, &next, Some(committed_position))?;
    Ok(next)
}

/// Metadata for the supported durable snapshot envelope.
///
/// Format 1 checksums the envelope excluding its trailing checksum; `total_bytes`
/// includes that checksum. These are storage semantics, not a peer protocol offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotDescriptor {
    pub format_version: u16,
    pub cluster_id: String,
    pub snapshot_id: String,
    pub last_included: Option<LogIdentity>,
    pub membership: Option<StoredMembership>,
    pub total_bytes: u64,
    pub checksum: [u8; 32],
}

impl SnapshotDescriptor {
    /// # Errors
    /// Rejects membership introduced beyond or conflicting with the boundary.
    pub fn validate_positions(&self) -> Result<(), PositionError> {
        validate_position_prefix(
            self.membership
                .as_ref()
                .and_then(|membership| membership.introduced_at),
            self.last_included,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub identity: LogIdentity,
    pub payload: EntryPayload,
}

/// A decoded append batch, admitted before persistence or engine mutation.
/// Authentication and negotiated transport limits remain adapter responsibilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendRequest {
    pub term: Term,
    pub leader: NodeId,
    pub previous: Option<LogIdentity>,
    pub entries: Vec<Entry>,
    pub leader_commit: Option<LogIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendError {
    TooManyEntries,
    ApplicationBytesExceeded,
    InvalidPreviousTerm,
    NonContiguousIndex,
    InvalidEntryTerm,
    ConflictingLeader,
    InvalidCommit,
}

impl AppendRequest {
    /// Validate the log chain without reading storage or changing authority.
    /// A leader commit may be beyond this batch; it is not permission to apply
    /// beyond the prefix actually matched and durably stored by the receiver.
    /// # Errors
    /// Rejects oversized batches, index gaps/overflow, and inconsistent identities.
    pub fn validate(
        &self,
        max_entries: usize,
        max_application_bytes: usize,
    ) -> Result<(), AppendError> {
        if self.entries.len() > max_entries {
            return Err(AppendError::TooManyEntries);
        }
        if self
            .previous
            .is_some_and(|previous| previous.term > self.term)
        {
            return Err(AppendError::InvalidPreviousTerm);
        }
        if self
            .previous
            .is_some_and(|previous| previous.term == self.term && previous.leader != self.leader)
        {
            return Err(AppendError::ConflictingLeader);
        }
        let mut previous = self.previous;
        let mut bytes = 0usize;
        for entry in &self.entries {
            let expected = match previous {
                Some(previous) => previous
                    .index
                    .successor()
                    .map_err(|_| AppendError::NonContiguousIndex)?,
                None => LogIndex(0),
            };
            if entry.identity.index != expected {
                return Err(AppendError::NonContiguousIndex);
            }
            if entry.identity.term > self.term
                || previous.is_some_and(|previous| previous.term > entry.identity.term)
            {
                return Err(AppendError::InvalidEntryTerm);
            }
            if (entry.identity.term == self.term && entry.identity.leader != self.leader)
                || previous.is_some_and(|previous| {
                    previous.term == entry.identity.term && previous.leader != entry.identity.leader
                })
            {
                return Err(AppendError::ConflictingLeader);
            }
            // Charge membership data as well as application bytes. This is a
            // decoded-model budget, not a claim about transport envelope size.
            let payload_bytes = match &entry.payload {
                EntryPayload::NoOp => 0,
                EntryPayload::Application(payload) => payload.len(),
                EntryPayload::Membership(membership) => membership
                    .admission_bytes()
                    .ok_or(AppendError::ApplicationBytesExceeded)?,
            };
            bytes = bytes
                .checked_add(payload_bytes)
                .filter(|bytes| *bytes <= max_application_bytes)
                .ok_or(AppendError::ApplicationBytesExceeded)?;
            previous = Some(entry.identity);
        }
        if let Some(commit) = self.leader_commit {
            if commit.term > self.term {
                return Err(AppendError::InvalidCommit);
            }
            for identity in self
                .previous
                .iter()
                .chain(self.entries.iter().map(|entry| &entry.identity))
            {
                let (prefix, boundary) = if commit.index <= identity.index {
                    (commit, *identity)
                } else {
                    (*identity, commit)
                };
                validate_position_prefix(Some(prefix), Some(boundary))
                    .map_err(|_| AppendError::InvalidCommit)?;
            }
        }
        Ok(())
    }
}

/// A reconciliation result, not a durable acknowledgement. The caller must
/// serialize its authoritative reads with application of the replacement suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendReconciliation {
    PreviousMismatch,
    /// Entries through this identity matched or are proposed for persistence.
    /// `replace_from` is an offset in the request, never a durable log index.
    Matched {
        through: Option<LogIdentity>,
        replace_from: Option<usize>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileError {
    InvalidRequest(AppendError),
    CommittedConflict,
    ConflictingPayload,
    MissingCommittedEntry,
    InvalidRecoveryPositions,
    CompactedPrefix,
    StorageReadFailure,
}

impl AppendRequest {
    /// Compare a bounded request against one serialized authoritative log view.
    /// The snapshot boundary and committed position come from the same recovered
    /// authority as lookup. Compacted entries are never reconstructed or guessed.
    /// # Errors
    /// Rejects malformed requests and changes to committed entries. Read failure
    /// stops reconciliation rather than being interpreted as a missing log entry.
    /// The adapter must stop serving authority after a storage failure.
    pub fn reconcile<'a>(
        &self,
        committed: Option<LogIdentity>,
        snapshot: Option<LogIdentity>,
        mut lookup: impl FnMut(LogIndex) -> Result<Option<&'a Entry>, ReconcileError>,
        max_entries: usize,
        max_application_bytes: usize,
    ) -> Result<AppendReconciliation, ReconcileError> {
        self.validate(max_entries, max_application_bytes)
            .map_err(ReconcileError::InvalidRequest)?;
        validate_position_prefix(snapshot, committed)
            .map_err(|_| ReconcileError::InvalidRecoveryPositions)?;
        if let Some(commit) = committed {
            let recovered = match snapshot {
                Some(boundary) if boundary.index == commit.index => Some(boundary),
                _ => lookup(commit.index)?.map(|entry| entry.identity),
            };
            match recovered {
                None => return Err(ReconcileError::MissingCommittedEntry),
                Some(identity) if identity != commit => {
                    return Err(ReconcileError::InvalidRecoveryPositions);
                }
                Some(_) => {}
            }
        }
        if let Some(boundary) = snapshot
            && self
                .previous
                .is_none_or(|previous| previous.index < boundary.index)
        {
            return Err(ReconcileError::CompactedPrefix);
        }
        if let Some(previous) = self.previous {
            let local = match snapshot {
                Some(boundary) if boundary.index == previous.index => Some(boundary),
                _ => lookup(previous.index)?.map(|entry| entry.identity),
            };
            if local != Some(previous) {
                return Ok(AppendReconciliation::PreviousMismatch);
            }
        }
        let mut replace_from = None;
        for (offset, proposed) in self.entries.iter().enumerate() {
            let existing = lookup(proposed.identity.index)?;
            let protected = committed.is_some_and(|commit| proposed.identity.index <= commit.index);
            match existing {
                Some(existing) if existing.identity == proposed.identity => {
                    if existing.payload != proposed.payload {
                        return Err(ReconcileError::ConflictingPayload);
                    }
                }
                Some(_) if protected => return Err(ReconcileError::CommittedConflict),
                None if protected => return Err(ReconcileError::MissingCommittedEntry),
                _ => {
                    replace_from.get_or_insert(offset);
                }
            }
        }
        Ok(AppendReconciliation::Matched {
            through: self
                .entries
                .last()
                .map(|entry| entry.identity)
                .or(self.previous),
            replace_from,
        })
    }
}

/// Application bytes keep their own schema and deduplication semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryPayload {
    NoOp,
    Application(Vec<u8>),
    Membership(Membership),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Term(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogIndex(pub u64);

/// Exhaustion requires stopping the affected authority, never wrapping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityExhaustion {
    Term,
    LogIndex,
}

impl Term {
    /// # Errors
    /// Returns an exhaustion condition at the largest representable term.
    pub fn successor(self) -> Result<Self, IdentityExhaustion> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(IdentityExhaustion::Term)
    }
}

impl LogIndex {
    /// # Errors
    /// Returns an exhaustion condition at the largest representable index.
    pub fn successor(self) -> Result<Self, IdentityExhaustion> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(IdentityExhaustion::LogIndex)
    }
}

/// Persisted voting authority; the legacy committed-vote flag is not a
/// commit position and remains solely in the storage adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardState {
    pub term: Term,
    pub voted_for: Option<NodeId>,
}

/// Combined append authority decision.
///
/// Neither outcome authorizes a reply until
/// required hard-state and suffix writes complete. Errors require stopping the
/// affected operation; storage errors additionally stop the authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendDecision {
    pub persist: Option<HardState>,
    pub term: Term,
    pub step_down: bool,
    /// Proposed commit watermark, published only after required persistence.
    /// This does not advance the separately tracked durable applied position.
    pub commit: Option<LogIdentity>,
    pub outcome: Result<AppendOutcome, ReconcileError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    StaleTerm,
    Reconciled(AppendReconciliation),
}

impl HardState {
    /// Evaluate an authenticated append against a serialized recovered log view.
    /// Higher-term authority must be persisted even on a log mismatch. `step_down`
    /// also directs candidates to follow a same-term authenticated leader.
    #[must_use]
    pub fn evaluate_append<'a>(
        self,
        request: &AppendRequest,
        positions: (Option<LogIdentity>, Option<LogIdentity>),
        mut lookup: impl FnMut(LogIndex) -> Result<Option<&'a Entry>, ReconcileError>,
        limits: (usize, usize),
    ) -> AppendDecision {
        if request.term < self.term {
            return AppendDecision {
                persist: None,
                term: self.term,
                step_down: false,
                commit: positions.0,
                outcome: Ok(AppendOutcome::StaleTerm),
            };
        }
        let persist = (request.term > self.term).then_some(Self {
            term: request.term,
            voted_for: None,
        });
        let mut outcome = request
            .reconcile(positions.0, positions.1, &mut lookup, limits.0, limits.1)
            .map(AppendOutcome::Reconciled);
        let mut commit = positions.0;
        if let Ok(AppendOutcome::Reconciled(AppendReconciliation::Matched {
            through: Some(matched),
            ..
        })) = outcome
            && let Some(advertised) = request.leader_commit
        {
            let candidate = if advertised.index <= matched.index {
                advertised
            } else {
                matched
            };
            if commit.is_none_or(|current| candidate.index > current.index) {
                let identity = if candidate == matched {
                    Ok(Some(matched))
                } else if let Some(entry) = request
                    .entries
                    .iter()
                    .find(|entry| entry.identity.index == candidate.index)
                {
                    Ok(Some(entry.identity))
                } else {
                    lookup(candidate.index).map(|entry| entry.map(|entry| entry.identity))
                };
                match identity {
                    Ok(Some(identity)) if identity == candidate => commit = Some(candidate),
                    Ok(_) => outcome = Err(ReconcileError::InvalidRecoveryPositions),
                    Err(error) => outcome = Err(error),
                }
            }
        }
        AppendDecision {
            persist,
            term: request.term,
            step_down: true,
            commit,
            outcome,
        }
    }
}

/// Full identity is used for matching; deliberately has no total ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogIdentity {
    pub term: Term,
    pub leader: NodeId,
    pub index: LogIndex,
}

impl LogIdentity {
    /// Compare candidate freshness by term then index, never leader `NodeId`.
    /// `None` denotes an empty log and is older than every present identity.
    #[must_use]
    pub fn compare_freshness(candidate: Option<Self>, local: Option<Self>) -> Ordering {
        let key = |identity: Self| (identity.term, identity.index);
        candidate.map(key).cmp(&local.map(key))
    }
}

/// Decoded engine input; adapters bind candidate to authenticated sender and
/// negotiate the engine protocol before constructing this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestVote {
    pub term: Term,
    pub candidate: NodeId,
    pub last_log: Option<LogIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteResponse {
    pub term: Term,
    pub granted: bool,
}

/// Execution-local leader progress, never recovered as leadership after restart.
/// Remote matches start unknown: election votes are not replication evidence.
///
/// Public as an input to the pure read-barrier model, like `Campaign`; runtime
/// ownership, persistence, and transport remain outside this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderProgress {
    pub term: Term,
    pub membership: Membership,
    pub matched: BTreeMap<NodeId, Option<LogIdentity>>,
    pub next: BTreeMap<NodeId, LogIndex>,
}

impl LeaderProgress {
    /// A conflict is a retry hint, never evidence that a prefix is durable.
    /// Ignore responses for probes superseded by a newer success or conflict.
    pub fn reject_probe(&mut self, peer: NodeId, previous: Option<LogIndex>, hint: LogIndex) {
        let Some(previous) = previous else { return };
        let Some(sent_next) = previous.0.checked_add(1).map(LogIndex) else {
            return;
        };
        let Some(next) = self.next.get_mut(&peer) else {
            return;
        };
        if *next != sent_next {
            return;
        }
        let floor = match self.matched.get(&peer).copied().flatten() {
            Some(matched) => {
                let Some(floor) = matched.index.0.checked_add(1).map(LogIndex) else {
                    return;
                };
                floor
            }
            None => LogIndex(0),
        };
        *next = hint.min(previous).max(floor);
    }

    pub fn acknowledge_probe(&mut self, peer: NodeId, matched: Option<LogIdentity>) {
        let Some(matched) = matched else { return };
        let Some(current) = self.matched.get_mut(&peer) else {
            return;
        };
        if current.is_none_or(|old| matched.index > old.index) {
            *current = Some(matched);
        }
        if let Some(next) = matched.index.0.checked_add(1).map(LogIndex)
            && let Some(current) = self.next.get_mut(&peer)
        {
            *current = (*current).max(next);
        }
    }
    /// Highest index covered by each voter majority. This is only a candidate;
    /// the storage owner must check the current-term rule and commit durably.
    #[must_use]
    pub fn quorum_index(&self) -> Option<LogIndex> {
        self.membership
            .configurations()
            .iter()
            .map(|voters| {
                let mut positions: Vec<_> = voters
                    .iter()
                    .filter_map(|peer| self.matched.get(peer).copied().flatten().map(|id| id.index))
                    .collect();
                let majority = voters.len() / 2 + 1;
                if positions.len() < majority {
                    return None;
                }
                positions.sort_unstable_by(|left, right| right.cmp(left));
                Some(positions[majority - 1])
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .min()
    }

    #[must_use]
    pub fn after_durable_initialization(membership: Membership, entry: LogIdentity) -> Self {
        let matched = membership
            .nodes()
            .keys()
            .map(|peer| (*peer, (*peer == entry.leader).then_some(entry)))
            .collect();
        let next = membership
            .nodes()
            .keys()
            .map(|peer| (*peer, entry.index))
            .collect();
        Self {
            term: entry.term,
            membership,
            matched,
            next,
        }
    }
}

/// Execution-local correlation, not a credential or a durable consensus record.
///
/// The transport must echo this context from an authenticated heartbeat reply.
/// `sequence` is allocated by `ReadBarrier`, never taken from a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadContext {
    pub sequence: u64,
    pub term: Term,
    pub leader: NodeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadConfirmation {
    pub context: ReadContext,
    pub term: Term,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadRound {
    context: ReadContext,
    membership: StoredMembership,
    committed: LogIdentity,
    acknowledged: BTreeSet<NodeId>,
}

impl ReadRound {
    fn is_current(
        &self,
        leader: Option<&LeaderProgress>,
        hard_state: HardState,
        membership: &StoredMembership,
    ) -> bool {
        leader.is_some_and(|leader| {
            leader.term == self.context.term
                && leader.membership == membership.membership
                && hard_state.term == self.context.term
                && hard_state.voted_for == Some(self.context.leader)
                && membership == &self.membership
        })
    }
}

/// Pure, single-flight read confirmation.
///
/// Keep one owner across leader changes; replacing it within a term could reuse
/// a sequence. Restart requires a new
/// election term before admitting reads. No clock, lease, or replication match
/// substitutes for fresh heartbeat acknowledgments.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadBarrier {
    next_sequence: Option<u64>,
    pending: Option<ReadRound>,
}

impl Default for ReadBarrier {
    fn default() -> Self {
        Self {
            next_sequence: Some(0),
            pending: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadSequenceExhausted;

impl ReadBarrier {
    /// Cancel on leadership loss or every effective-configuration publication,
    /// including a change that later restores the same configuration. Preserve
    /// the sequence allocator so delayed replies never revive a cancelled read.
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// Start after the existing leader no-op (or another current-term entry)
    /// commits durably. Supersedes any unfinished read, including on rejection.
    /// `membership` is the latest durable effective configuration, not merely
    /// the committed configuration. Positions come from authoritative storage.
    /// # Errors
    /// Sequence exhaustion requires a new owner/election, never identity reuse.
    pub fn begin(
        &mut self,
        leader: &LeaderProgress,
        hard_state: HardState,
        membership: &StoredMembership,
        committed: Option<LogIdentity>,
    ) -> Result<Option<ReadContext>, ReadSequenceExhausted> {
        self.pending = None;
        let Some(committed) = committed else {
            return Ok(None);
        };
        if committed.term != leader.term
            || hard_state.term != leader.term
            || hard_state.voted_for != Some(committed.leader)
            || membership.membership != leader.membership
            || validate_position_prefix(
                Some(committed),
                leader.matched.get(&committed.leader).copied().flatten(),
            )
            .is_err()
        {
            return Ok(None);
        }
        let sequence = self.next_sequence.ok_or(ReadSequenceExhausted)?;
        self.next_sequence = sequence.checked_add(1);
        let context = ReadContext {
            sequence,
            term: leader.term,
            leader: committed.leader,
        };
        self.pending = Some(ReadRound {
            context,
            membership: membership.clone(),
            committed,
            acknowledged: BTreeSet::from([committed.leader]),
        });
        Ok(Some(context))
    }

    /// Count only authenticated voter replies for this exact round and term.
    /// Returns a higher eligible-peer term for the adapter's normal durable
    /// step-down path; this never persists or grants authority. Even an old
    /// round's higher-term reply invalidates pending reads. Ordinary same-term
    /// stale, foreign, rejected, and duplicate replies cannot add quorum weight.
    pub fn observe(
        &mut self,
        leader: Option<&LeaderProgress>,
        hard_state: HardState,
        membership: &StoredMembership,
        peer: NodeId,
        response: ReadConfirmation,
    ) -> Option<Term> {
        // Authority changes cancel even when this particular reply is ineligible.
        // Do not let a later restoration revive evidence from the old round.
        if self
            .pending
            .as_ref()
            .is_some_and(|round| !round.is_current(leader, hard_state, membership))
        {
            self.cancel();
        }
        if !membership
            .membership
            .configurations()
            .iter()
            .any(|voters| voters.contains(&peer))
        {
            return None;
        }
        if response.term > hard_state.term {
            self.pending = None;
            return Some(response.term);
        }
        let round = self.pending.as_mut()?;
        if response.context == round.context
            && response.term == round.context.term
            && response.accepted
        {
            round.acknowledged.insert(peer);
        }
        None
    }

    /// Consume read authority once, only after application reaches the captured
    /// committed watermark. Revalidate leadership and effective membership even
    /// after quorum confirmation. Missing leadership cancels the round.
    /// The returned position labels the authorized read; callers must read the
    /// corresponding applied state in the same serialized authority scope.
    pub fn complete(
        &mut self,
        leader: Option<&LeaderProgress>,
        hard_state: HardState,
        membership: &StoredMembership,
        committed: Option<LogIdentity>,
        applied: Option<LogIdentity>,
    ) -> Option<LogIdentity> {
        let round = self.pending.as_ref()?;
        if !round.is_current(leader, hard_state, membership)
            || validate_position_prefix(Some(round.committed), committed).is_err()
            || validate_position_prefix(applied, committed).is_err()
        {
            self.pending = None;
            return None;
        }
        if !round.membership.membership.has_quorum(&round.acknowledged)
            || validate_position_prefix(Some(round.committed), applied).is_err()
        {
            return None;
        }
        self.pending.take().map(|round| round.committed)
    }
}

/// Caller-owned election state, created only after the self-vote is durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Campaign {
    pub(crate) request: RequestVote,
    pub(crate) membership: Membership,
    granted: BTreeSet<NodeId>,
}

/// A response transition. Higher-term persistence must complete before any
/// dependent effect; `quorum` is not permission to acknowledge application work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CampaignDecision {
    pub persist: Option<HardState>,
    pub quorum: bool,
}

impl Campaign {
    /// Plan the first leader entry against current durable authority. This is
    /// not a commit decision: replication must still establish a current quorum.
    /// # Errors
    /// Log-index exhaustion requires explicit recovery rather than wraparound.
    pub fn leader_initialization(
        &self,
        hard_state: HardState,
        membership: &Membership,
        last_log: Option<LogIdentity>,
    ) -> Result<Option<AppendRequest>, IdentityExhaustion> {
        if hard_state.term != self.request.term
            || hard_state.voted_for != Some(self.request.candidate)
            || membership != &self.membership
            || !membership.has_quorum(&self.granted)
            || last_log.is_some_and(|last| last.term > hard_state.term)
        {
            return Ok(None);
        }
        let index = last_log.map_or(Ok(LogIndex(0)), |last| last.index.successor())?;
        Ok(Some(AppendRequest {
            term: hard_state.term,
            leader: self.request.candidate,
            previous: last_log,
            entries: vec![Entry {
                identity: LogIdentity {
                    term: hard_state.term,
                    leader: self.request.candidate,
                    index,
                },
                payload: EntryPayload::NoOp,
            }],
            leader_commit: None,
        }))
    }

    #[must_use]
    pub fn new(request: RequestVote, membership: Membership) -> Self {
        Self {
            request,
            membership,
            granted: BTreeSet::from([request.candidate]),
        }
    }

    /// Evaluate an authenticated, request-bound response using current durable
    /// authority. No clocks, storage, or network operations occur here.
    /// Higher terms from eligible peers invalidate even an absent/old campaign.
    pub fn observe(
        campaign: &mut Option<Self>,
        hard_state: HardState,
        membership: Option<&Membership>,
        campaign_term: Term,
        peer: NodeId,
        response: VoteResponse,
    ) -> CampaignDecision {
        let ignored = CampaignDecision {
            persist: None,
            quorum: false,
        };
        let Some(membership) = membership.filter(|membership| {
            membership
                .configurations()
                .iter()
                .any(|voters| voters.contains(&peer))
        }) else {
            return ignored;
        };
        if response.term > hard_state.term {
            *campaign = None;
            return CampaignDecision {
                persist: Some(HardState {
                    term: response.term,
                    voted_for: None,
                }),
                quorum: false,
            };
        }
        let Some(active) = campaign else {
            return ignored;
        };
        if active.request.term != campaign_term {
            return ignored;
        }
        if hard_state.term != campaign_term
            || hard_state.voted_for != Some(active.request.candidate)
            || membership != &active.membership
        {
            *campaign = None;
            return ignored;
        }
        if response.term != campaign_term || !response.granted {
            return ignored;
        }
        active.granted.insert(peer);
        CampaignDecision {
            persist: None,
            quorum: active.membership.has_quorum(&active.granted),
        }
    }
}

/// A proposed transition, not permission to send a reply. If persistence is
/// present, its durable completion must precede publication of the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteDecision {
    pub persist: Option<HardState>,
    pub response: VoteResponse,
}

impl HardState {
    /// Evaluate one authenticated vote request without performing any effects.
    /// The caller serializes this with pending hard-state writes and steps down
    /// on higher terms. Membership is the effective durable-log configuration.
    #[must_use]
    pub fn evaluate_vote(
        self,
        local: NodeId,
        last_log: Option<LogIdentity>,
        membership: &Membership,
        request: RequestVote,
    ) -> VoteDecision {
        let mut next = self;
        if request.term > next.term {
            next = Self {
                term: request.term,
                voted_for: None,
            };
        }
        let voter = |id| {
            membership
                .configurations()
                .iter()
                .any(|set| set.contains(&id))
        };
        let plausible_log = request
            .last_log
            .is_none_or(|identity| identity.term <= request.term);
        let granted = request.term == next.term
            && voter(local)
            && voter(request.candidate)
            && plausible_log
            && next
                .voted_for
                .is_none_or(|candidate| candidate == request.candidate)
            && LogIdentity::compare_freshness(request.last_log, last_log) != Ordering::Less;
        if granted {
            next.voted_for = Some(request.candidate);
        }
        VoteDecision {
            persist: (next != self).then_some(next),
            response: VoteResponse {
                term: next.term,
                granted,
            },
        }
    }
}

/// Identity allocated by the caller for each runtime incarnation, plus a
/// non-reused sequence within it. Neither field is a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteEffectId {
    pub incarnation: u128,
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteAction {
    Persist {
        id: VoteEffectId,
        hard_state: HardState,
    },
    Reply(VoteResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteGateError {
    Busy,
    Stopped,
    EffectExhausted,
    StorageFailure,
}

/// Serializes vote persistence without I/O or an internal queue.
///
/// The enclosing engine must also serialize log/configuration changes and term
/// updates with this gate. It is not independently a complete Raft engine.
#[derive(Debug)]
pub struct VoteGate {
    durable: HardState,
    incarnation: u128,
    sequence: u64,
    pending: Option<(VoteEffectId, HardState, VoteResponse)>,
    stopped: bool,
}

impl VoteGate {
    /// The caller supplies a fresh incarnation and recovered durable authority.
    #[must_use]
    pub const fn new(incarnation: u128, durable: HardState) -> Self {
        Self {
            durable,
            incarnation,
            sequence: 0,
            pending: None,
            stopped: false,
        }
    }

    /// # Errors
    /// Applies explicit backpressure while a write is pending; exhaustion stops
    /// this incarnation instead of reusing a completion identity.
    pub fn request(
        &mut self,
        local: NodeId,
        last_log: Option<LogIdentity>,
        membership: &Membership,
        request: RequestVote,
    ) -> Result<VoteAction, VoteGateError> {
        if self.stopped {
            return Err(VoteGateError::Stopped);
        }
        if self.pending.is_some() {
            return Err(VoteGateError::Busy);
        }
        let decision = self
            .durable
            .evaluate_vote(local, last_log, membership, request);
        let Some(hard_state) = decision.persist else {
            return Ok(VoteAction::Reply(decision.response));
        };
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.stopped = true;
            return Err(VoteGateError::EffectExhausted);
        };
        self.sequence = sequence;
        let id = VoteEffectId {
            incarnation: self.incarnation,
            sequence,
        };
        self.pending = Some((id, hard_state, decision.response));
        Ok(VoteAction::Persist { id, hard_state })
    }

    /// Complete only the matching durable write. Late/duplicate completions do
    /// not advance authority or release a reply. Success means committed storage,
    /// not queue admission; cancellation with uncertain durability is failure.
    /// # Errors
    /// Storage failure permanently stops this incarnation; recovery needs a new one.
    pub fn complete(
        &mut self,
        id: VoteEffectId,
        durable_success: bool,
    ) -> Result<Option<VoteResponse>, VoteGateError> {
        if self.stopped {
            return Err(VoteGateError::Stopped);
        }
        let Some((expected, hard_state, response)) = self.pending else {
            return Ok(None);
        };
        if id != expected {
            return Ok(None);
        }
        self.pending = None;
        if !durable_success {
            self.stopped = true;
            return Err(VoteGateError::StorageFailure);
        }
        self.durable = hard_state;
        Ok(Some(response))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn append_budget_charges_membership_and_application_together() {
        let leader = NodeId::from(1);
        let membership = Membership::new(vec![vec![leader]], vec![(leader, "é".into())]).unwrap();
        let cost = 16 + 8 + 32 + 40 + 2;
        assert_eq!(membership.admission_bytes(), Some(cost));
        let identity = LogIdentity {
            term: Term(1),
            leader,
            index: LogIndex(0),
        };
        let mut request = AppendRequest {
            term: Term(1),
            leader,
            previous: None,
            entries: vec![Entry {
                identity,
                payload: EntryPayload::Membership(membership),
            }],
            leader_commit: None,
        };
        assert_eq!(request.validate(2, cost), Ok(()));
        assert_eq!(
            request.validate(2, cost - 1),
            Err(AppendError::ApplicationBytesExceeded)
        );
        request.entries.push(Entry {
            identity: LogIdentity {
                index: LogIndex(1),
                ..identity
            },
            payload: EntryPayload::Application(vec![0]),
        });
        assert_eq!(
            request.validate(2, cost),
            Err(AppendError::ApplicationBytesExceeded)
        );
        assert_eq!(request.validate(2, cost + 1), Ok(()));
    }
    #[test]
    fn follower_commit_is_capped_by_matched_request_not_local_tail() {
        let leader = NodeId::from(1);
        let first = LogIdentity {
            term: Term(1),
            leader,
            index: LogIndex(0),
        };
        let second = LogIdentity {
            index: LogIndex(1),
            ..first
        };
        let tail = LogIdentity {
            index: LogIndex(10),
            ..first
        };
        let entry = Entry {
            identity: first,
            payload: EntryPayload::NoOp,
        };
        let state = HardState {
            term: Term(2),
            voted_for: None,
        };
        let mut request = AppendRequest {
            term: Term(2),
            leader,
            previous: Some(first),
            entries: vec![Entry {
                identity: second,
                payload: EntryPayload::NoOp,
            }],
            leader_commit: Some(tail),
        };
        let decision = state.evaluate_append(
            &request,
            (Some(first), None),
            |index| Ok((index == first.index).then_some(&entry)),
            (1, 0),
        );
        assert_eq!(decision.commit, Some(second));
        request.entries.clear();
        let heartbeat =
            state.evaluate_append(&request, (Some(first), None), |_| Ok(Some(&entry)), (1, 0));
        assert_eq!(heartbeat.commit, Some(first));
        request.leader_commit = None;
        let unchanged =
            state.evaluate_append(&request, (Some(first), None), |_| Ok(Some(&entry)), (1, 0));
        assert_eq!(unchanged.commit, Some(first));
    }
    #[test]
    fn heartbeat_rejects_missing_or_conflicting_committed_authority() {
        let leader = NodeId::from(1);
        let commit = LogIdentity {
            term: Term(2),
            leader,
            index: LogIndex(0),
        };
        let request = AppendRequest {
            term: Term(3),
            leader,
            previous: None,
            entries: vec![],
            leader_commit: None,
        };
        assert_eq!(
            request.reconcile(Some(commit), None, |_| Ok(None), 0, 0),
            Err(ReconcileError::MissingCommittedEntry)
        );
        let corrupt = Entry {
            identity: LogIdentity {
                term: Term(1),
                ..commit
            },
            payload: EntryPayload::NoOp,
        };
        assert_eq!(
            request.reconcile(Some(commit), None, |_| Ok(Some(&corrupt)), 0, 0),
            Err(ReconcileError::InvalidRecoveryPositions)
        );
        assert_eq!(
            request.reconcile(
                Some(commit),
                None,
                |_| Err(ReconcileError::StorageReadFailure),
                0,
                0
            ),
            Err(ReconcileError::StorageReadFailure)
        );
    }
    #[test]
    fn append_term_transition_survives_mismatch_and_stale_requests_do_not_read() {
        let leader = NodeId::from(1);
        let state = HardState {
            term: Term(3),
            voted_for: Some(leader),
        };
        let mut request = AppendRequest {
            term: Term(2),
            leader,
            previous: None,
            entries: vec![],
            leader_commit: None,
        };
        let rejected = state.evaluate_append(
            &request,
            (None, None),
            |_| panic!("stale append read storage"),
            (1, 0),
        );
        assert_eq!(rejected.outcome, Ok(AppendOutcome::StaleTerm));
        assert_eq!(rejected.persist, None);
        assert!(!rejected.step_down);
        request.term = Term(4);
        request.previous = Some(LogIdentity {
            term: Term(2),
            leader,
            index: LogIndex(0),
        });
        let decision = state.evaluate_append(&request, (None, None), |_| Ok(None), (1, 0));
        assert_eq!(
            decision.persist,
            Some(HardState {
                term: Term(4),
                voted_for: None
            })
        );
        assert_eq!(
            decision.outcome,
            Ok(AppendOutcome::Reconciled(
                AppendReconciliation::PreviousMismatch
            ))
        );
        assert!(decision.step_down);
        let failed = state.evaluate_append(
            &request,
            (None, None),
            |_| Err(ReconcileError::StorageReadFailure),
            (1, 0),
        );
        assert_eq!(failed.persist, decision.persist);
        assert_eq!(failed.outcome, Err(ReconcileError::StorageReadFailure));
        request.term = Term(3);
        let same = state.evaluate_append(&request, (None, None), |_| Ok(None), (1, 0));
        assert_eq!(same.persist, None);
        assert!(same.step_down);
        assert_eq!(state.voted_for, Some(leader));
    }
    #[test]
    fn failed_lookup_never_becomes_mismatch_or_suffix_replacement() {
        let leader = NodeId::from(1);
        let identity = LogIdentity {
            term: Term(1),
            leader,
            index: LogIndex(0),
        };
        let mut request = AppendRequest {
            term: Term(1),
            leader,
            previous: Some(identity),
            entries: vec![],
            leader_commit: None,
        };
        assert_eq!(
            request.reconcile(
                None,
                None,
                |_| Err(ReconcileError::StorageReadFailure),
                1,
                0
            ),
            Err(ReconcileError::StorageReadFailure)
        );
        request.previous = None;
        request.entries.push(Entry {
            identity,
            payload: EntryPayload::NoOp,
        });
        assert_eq!(
            request.reconcile(
                None,
                None,
                |_| Err(ReconcileError::StorageReadFailure),
                1,
                0
            ),
            Err(ReconcileError::StorageReadFailure)
        );
    }
    #[test]
    fn reconciliation_uses_snapshot_identity_without_reading_deleted_entries() {
        let leader = NodeId::from(1);
        let boundary = LogIdentity {
            term: Term(2),
            leader,
            index: LogIndex(10),
        };
        let mut request = AppendRequest {
            term: Term(2),
            leader,
            previous: Some(boundary),
            entries: vec![],
            leader_commit: Some(boundary),
        };
        let no_read = |_| -> Result<Option<&Entry>, ReconcileError> {
            panic!("compacted history must not be read")
        };
        assert_eq!(
            request.reconcile(Some(boundary), Some(boundary), no_read, 1, 0),
            Ok(AppendReconciliation::Matched {
                through: Some(boundary),
                replace_from: None
            })
        );
        assert_eq!(
            request.reconcile(None, Some(boundary), no_read, 1, 0),
            Err(ReconcileError::InvalidRecoveryPositions)
        );
        request.previous = None;
        assert_eq!(
            request.reconcile(Some(boundary), Some(boundary), no_read, 1, 0),
            Err(ReconcileError::CompactedPrefix)
        );
        request.previous = Some(LogIdentity {
            index: LogIndex(9),
            ..boundary
        });
        assert_eq!(
            request.reconcile(Some(boundary), Some(boundary), no_read, 1, 0),
            Err(ReconcileError::CompactedPrefix)
        );
        request.previous = Some(boundary);
        let next = LogIdentity {
            index: LogIndex(11),
            ..boundary
        };
        request.entries.push(Entry {
            identity: next,
            payload: EntryPayload::NoOp,
        });
        assert_eq!(
            request.reconcile(Some(boundary), Some(boundary), |_| Ok(None), 1, 0),
            Ok(AppendReconciliation::Matched {
                through: Some(next),
                replace_from: Some(0)
            })
        );
    }
    #[test]
    fn append_reconciliation_protects_committed_prefix_and_preserves_extra_tail() {
        let leader = NodeId::from(1);
        let first = Entry {
            identity: LogIdentity {
                term: Term(1),
                leader,
                index: LogIndex(0),
            },
            payload: EntryPayload::NoOp,
        };
        let old = Entry {
            identity: LogIdentity {
                index: LogIndex(1),
                ..first.identity
            },
            payload: EntryPayload::NoOp,
        };
        let replacement = Entry {
            identity: LogIdentity {
                term: Term(2),
                ..old.identity
            },
            payload: EntryPayload::NoOp,
        };
        let mut request = AppendRequest {
            term: Term(2),
            leader,
            previous: Some(first.identity),
            entries: vec![replacement.clone()],
            leader_commit: Some(first.identity),
        };
        let log = [&first, &old];
        let lookup = |index: LogIndex| {
            Ok(log
                .iter()
                .copied()
                .find(|entry| entry.identity.index == index))
        };
        assert_eq!(
            request.reconcile(Some(first.identity), None, lookup, 1, 0),
            Ok(AppendReconciliation::Matched {
                through: Some(replacement.identity),
                replace_from: Some(0)
            })
        );
        assert_eq!(
            request.reconcile(Some(old.identity), None, lookup, 1, 0),
            Err(ReconcileError::CommittedConflict)
        );
        request.entries.clear();
        assert_eq!(
            request.reconcile(Some(first.identity), None, lookup, 1, 0),
            Ok(AppendReconciliation::Matched {
                through: Some(first.identity),
                replace_from: None
            })
        );
        request.previous = Some(replacement.identity);
        assert_eq!(
            request.reconcile(Some(first.identity), None, lookup, 1, 0),
            Ok(AppendReconciliation::PreviousMismatch)
        );
    }

    #[test]
    fn append_admission_checks_chain_and_budget_before_effects() {
        let leader = NodeId::from(1);
        let first = LogIdentity {
            term: Term(2),
            leader,
            index: LogIndex(0),
        };
        let mut request = AppendRequest {
            term: Term(2),
            leader,
            previous: None,
            entries: vec![Entry {
                identity: first,
                payload: EntryPayload::Application(vec![1, 2]),
            }],
            leader_commit: Some(first),
        };
        assert_eq!(request.validate(1, 2), Ok(()));
        assert_eq!(request.validate(0, 2), Err(AppendError::TooManyEntries));
        assert_eq!(
            request.validate(1, 1),
            Err(AppendError::ApplicationBytesExceeded)
        );
        request.entries[0].identity.index = LogIndex(1);
        assert_eq!(request.validate(1, 2), Err(AppendError::NonContiguousIndex));
        request.entries[0].identity = first;
        request.entries[0].identity.leader = NodeId::from(2);
        assert_eq!(request.validate(1, 2), Err(AppendError::ConflictingLeader));
        request.entries[0].identity = first;
        request.leader_commit = Some(LogIdentity {
            term: Term(1),
            ..first
        });
        assert_eq!(request.validate(1, 2), Err(AppendError::InvalidCommit));
        request.leader_commit = Some(LogIdentity {
            index: LogIndex(10),
            ..first
        });
        assert_eq!(request.validate(1, 2), Ok(()));
        request.previous = Some(LogIdentity {
            index: LogIndex(u64::MAX),
            ..first
        });
        assert_eq!(request.validate(1, 2), Err(AppendError::NonContiguousIndex));
    }

    use super::*;

    #[test]
    fn vote_gate_releases_only_matching_durable_completion() {
        let node = NodeId::from(1);
        let membership = Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
        let original = HardState {
            term: Term(0),
            voted_for: None,
        };
        let mut gate = VoteGate::new(41, original);
        let request = RequestVote {
            term: Term(1),
            candidate: node,
            last_log: None,
        };
        let VoteAction::Persist { id, hard_state } =
            gate.request(node, None, &membership, request).unwrap()
        else {
            panic!("reply before persistence");
        };
        assert_eq!(gate.durable, original);
        assert_eq!(
            gate.request(node, None, &membership, request),
            Err(VoteGateError::Busy)
        );
        assert_eq!(
            gate.complete(
                VoteEffectId {
                    incarnation: 40,
                    ..id
                },
                true
            ),
            Ok(None)
        );
        assert_eq!(gate.durable, original);
        assert!(gate.complete(id, true).unwrap().unwrap().granted);
        assert_eq!(gate.durable, hard_state);
        assert_eq!(gate.complete(id, true), Ok(None));
        assert!(matches!(
            gate.request(node, None, &membership, request).unwrap(),
            VoteAction::Reply(VoteResponse { granted: true, .. })
        ));
        let VoteAction::Persist { id: next, .. } = gate
            .request(
                node,
                None,
                &membership,
                RequestVote {
                    term: Term(2),
                    ..request
                },
            )
            .unwrap()
        else {
            panic!("missing term persistence");
        };
        assert_ne!(id, next);
        assert_eq!(gate.complete(id, false), Ok(None));
        assert_eq!(
            gate.complete(next, false),
            Err(VoteGateError::StorageFailure)
        );
        assert_eq!(gate.complete(next, true), Err(VoteGateError::Stopped));
        assert_eq!(
            gate.request(node, None, &membership, request),
            Err(VoteGateError::Stopped)
        );
        let mut exhausted = VoteGate::new(42, original);
        exhausted.sequence = u64::MAX;
        assert_eq!(
            exhausted.request(node, None, &membership, request),
            Err(VoteGateError::EffectExhausted)
        );
    }

    #[test]
    fn vote_decisions_preserve_term_and_single_candidate_authority() {
        let local = NodeId::from(1);
        let candidate = NodeId::from(2);
        let other = NodeId::from(3);
        let membership = Membership::new(
            vec![vec![local, candidate, other]],
            vec![
                (local, String::new()),
                (candidate, String::new()),
                (other, String::new()),
            ],
        )
        .unwrap();
        let state = HardState {
            term: Term(1),
            voted_for: None,
        };
        let request = RequestVote {
            term: Term(2),
            candidate,
            last_log: None,
        };
        let decision = state.evaluate_vote(local, None, &membership, request);
        assert!(decision.response.granted);
        let persisted = decision.persist.unwrap();
        assert_eq!(
            persisted,
            HardState {
                term: Term(2),
                voted_for: Some(candidate)
            }
        );
        let retry = persisted.evaluate_vote(local, None, &membership, request);
        assert!(retry.response.granted);
        assert_eq!(retry.persist, None);
        let conflict = persisted.evaluate_vote(
            local,
            None,
            &membership,
            RequestVote {
                candidate: other,
                ..request
            },
        );
        assert!(!conflict.response.granted);
        assert_eq!(conflict.persist, None);
        let obsolete = persisted.evaluate_vote(
            local,
            None,
            &membership,
            RequestVote {
                term: Term(1),
                ..request
            },
        );
        assert!(!obsolete.response.granted);
        assert_eq!(obsolete.response.term, Term(2));
        let log = LogIdentity {
            term: Term(2),
            index: LogIndex(0),
            leader: candidate,
        };
        let newer = persisted.evaluate_vote(
            local,
            Some(log),
            &membership,
            RequestVote {
                term: Term(3),
                ..request
            },
        );
        assert!(!newer.response.granted);
        assert_eq!(
            newer.persist,
            Some(HardState {
                term: Term(3),
                voted_for: None
            })
        );
        let outsider = state.evaluate_vote(
            local,
            None,
            &membership,
            RequestVote {
                candidate: NodeId::from(4),
                ..request
            },
        );
        assert!(!outsider.response.granted);
        let learner = state.evaluate_vote(NodeId::from(4), None, &membership, request);
        assert!(!learner.response.granted);
    }

    #[test]
    fn membership_requires_both_majorities_and_valid_records() {
        let ids: Vec<_> = (1..=6).map(NodeId::from).collect();
        let records = || ids.iter().map(|id| (*id, String::new())).collect();
        let configuration =
            Membership::new(vec![ids[..3].to_vec(), ids[2..5].to_vec()], records()).unwrap();
        assert!(!configuration.has_quorum(&BTreeSet::from([ids[0], ids[1], ids[5]])));
        assert!(configuration.has_quorum(&BTreeSet::from([ids[0], ids[2], ids[3]])));
        assert!(!configuration.has_quorum(&BTreeSet::new()));
        assert_eq!(
            Membership::new(vec![vec![ids[0], ids[0]]], records()),
            Err(MembershipError::DuplicateVoter)
        );
        assert_eq!(
            Membership::new(vec![vec![ids[0]]], vec![]),
            Err(MembershipError::MissingNode)
        );
        assert_eq!(
            Membership::new(vec![vec![]], records()),
            Err(MembershipError::EmptyVoters)
        );
        assert_eq!(
            Membership::new(vec![], records()),
            Err(MembershipError::ConfigurationCount)
        );
        assert_eq!(
            Membership::new(
                vec![vec![ids[0]]],
                vec![(ids[0], String::new()), (ids[0], String::new())]
            ),
            Err(MembershipError::DuplicateNode)
        );
    }

    #[test]
    fn campaign_transition_requires_joint_quorum_and_durable_authority() {
        let ids: Vec<_> = (1..=5).map(NodeId::from).collect();
        let membership = Membership::new(
            vec![ids[..3].to_vec(), ids[2..].to_vec()],
            ids.iter().map(|id| (*id, String::new())).collect(),
        )
        .unwrap();
        let request = RequestVote {
            term: Term(2),
            candidate: ids[0],
            last_log: None,
        };
        let hard = HardState {
            term: request.term,
            voted_for: Some(ids[0]),
        };
        let mut campaign = Some(Campaign::new(request, membership.clone()));
        for (peer, quorum) in [(ids[1], false), (ids[2], false), (ids[3], true)] {
            let decision = Campaign::observe(
                &mut campaign,
                hard,
                Some(&membership),
                Term(2),
                peer,
                VoteResponse {
                    term: Term(2),
                    granted: true,
                },
            );
            assert_eq!(
                decision,
                CampaignDecision {
                    persist: None,
                    quorum
                }
            );
        }
        let stale = Campaign::observe(
            &mut campaign,
            hard,
            Some(&membership),
            Term(1),
            ids[4],
            VoteResponse {
                term: Term(1),
                granted: true,
            },
        );
        assert!(!stale.quorum);
        let higher = Campaign::observe(
            &mut campaign,
            hard,
            Some(&membership),
            Term(1),
            ids[4],
            VoteResponse {
                term: Term(3),
                granted: false,
            },
        );
        assert_eq!(
            higher.persist,
            Some(HardState {
                term: Term(3),
                voted_for: None
            })
        );
        assert!(campaign.is_none());
        campaign = Some(Campaign::new(request, membership.clone()));
        let replacement = Membership::new(
            vec![ids.clone()],
            ids.iter().map(|id| (*id, String::new())).collect(),
        )
        .unwrap();
        let changed = Campaign::observe(
            &mut campaign,
            hard,
            Some(&replacement),
            Term(2),
            ids[1],
            VoteResponse {
                term: Term(2),
                granted: true,
            },
        );
        assert!(!changed.quorum);
        assert!(campaign.is_none());
    }

    #[test]
    fn replication_conflicts_backtrack_without_erasing_durable_matches() {
        let local = NodeId::from(1);
        let peer = NodeId::from(2);
        let identity = |index| LogIdentity {
            term: Term(3),
            leader: local,
            index: LogIndex(index),
        };
        let membership = Membership::new(
            vec![vec![local, peer]],
            vec![(local, String::new()), (peer, String::new())],
        )
        .unwrap();
        let mut leader = LeaderProgress::after_durable_initialization(membership, identity(10));
        leader.reject_probe(peer, Some(LogIndex(9)), LogIndex(500));
        assert_eq!(leader.next[&peer], LogIndex(9));
        assert_eq!(leader.matched[&peer], None);
        leader.reject_probe(peer, Some(LogIndex(9)), LogIndex(0));
        assert_eq!(leader.next[&peer], LogIndex(9), "stale conflict ignored");
        leader.reject_probe(peer, Some(LogIndex(8)), LogIndex(2));
        assert_eq!(leader.next[&peer], LogIndex(2));
        leader.acknowledge_probe(peer, Some(identity(7)));
        assert_eq!(leader.next[&peer], LogIndex(8));
        leader.reject_probe(peer, Some(LogIndex(7)), LogIndex(0));
        assert_eq!(
            leader.next[&peer],
            LogIndex(8),
            "never retry below durable match"
        );
        leader.acknowledge_probe(peer, Some(identity(3)));
        assert_eq!(leader.matched[&peer], Some(identity(7)));
        leader.reject_probe(peer, None, LogIndex(0));
        assert_eq!(leader.next[&peer], LogIndex(8));
    }

    #[test]
    fn replication_quorum_requires_each_majority_and_excludes_learners() {
        let nodes: Vec<_> = (1..=6).map(NodeId::from).collect();
        let membership = Membership::new(
            vec![nodes[..3].to_vec(), nodes[2..5].to_vec()],
            nodes.iter().map(|id| (*id, String::new())).collect(),
        )
        .unwrap();
        let identity = LogIdentity {
            term: Term(2),
            leader: nodes[0],
            index: LogIndex(8),
        };
        let mut progress = LeaderProgress::after_durable_initialization(membership, identity);
        progress.matched.insert(nodes[1], Some(identity));
        progress.matched.insert(nodes[5], Some(identity));
        assert_eq!(progress.quorum_index(), None);
        progress.matched.insert(nodes[2], Some(identity));
        assert_eq!(progress.quorum_index(), None);
        progress.matched.insert(
            nodes[3],
            Some(LogIdentity {
                index: LogIndex(5),
                ..identity
            }),
        );
        assert_eq!(progress.quorum_index(), Some(LogIndex(5)));
    }

    fn transition_record(index: u64, voters: Vec<Vec<u64>>) -> StoredMembership {
        StoredMembership {
            introduced_at: Some(LogIdentity {
                term: Term(3),
                leader: NodeId::from(1),
                index: LogIndex(index),
            }),
            membership: Membership::new(
                voters
                    .into_iter()
                    .map(|set| set.into_iter().map(NodeId::from).collect())
                    .collect(),
                (1..=5)
                    .map(|id| (NodeId::from(id), String::new()))
                    .collect(),
            )
            .unwrap(),
        }
    }

    #[test]
    fn membership_transition_accepts_lagging_follower_joint_and_final_batch() {
        let old = transition_record(10, vec![vec![1, 2, 3]]);
        let joint = transition_record(12, vec![vec![1, 2, 3], vec![2, 3, 4]]);
        let final_config = transition_record(14, vec![vec![2, 3, 4]]);
        assert_eq!(
            validate_membership_sequence(&old, [&joint, &final_config], joint.introduced_at),
            Ok(())
        );
        assert_eq!(
            validate_membership_sequence(&old, [&joint, &final_config], old.introduced_at),
            Err(MembershipTransitionError::PredecessorNotCommitted),
            "final cannot be justified by the follower's stale commit"
        );
        assert_eq!(
            validate_membership_sequence(&old, [&joint], old.introduced_at),
            Ok(())
        );
        // A later application entry can confirm the joint without itself being
        // supplied as a membership record.
        let later = LogIdentity {
            index: LogIndex(13),
            ..joint.introduced_at.unwrap()
        };
        assert_eq!(
            validate_membership_sequence(&old, [&joint, &final_config], Some(later)),
            Ok(())
        );
        let forged = LogIdentity {
            leader: NodeId::from(5),
            ..joint.introduced_at.unwrap()
        };
        assert_eq!(
            validate_membership_sequence(&old, [&joint, &final_config], Some(forged)),
            Err(MembershipTransitionError::ConflictingIdentity)
        );
    }

    #[test]
    fn membership_transition_follower_coverage_is_not_proposal_authority() {
        let old = transition_record(10, vec![vec![1, 2, 3]]);
        let joint = transition_record(12, vec![vec![1, 2, 3], vec![2, 3, 4]]);
        let final_config = transition_record(14, vec![vec![2, 3, 4]]);
        let next_joint = transition_record(16, vec![vec![2, 3, 4], vec![3, 4, 5]]);
        // A lagging follower can receive both entries with the leader's verified
        // joint commit. That coverage does NOT let a leader whose authoritative
        // committed membership is still old propose a final configuration.
        assert_eq!(
            validate_membership_sequence(&old, [&joint, &final_config], joint.introduced_at),
            Ok(())
        );
        let plan = |effective: &StoredMembership,
                    committed: &StoredMembership,
                    next: &StoredMembership| {
            plan_membership_successor(
                effective,
                committed,
                effective,
                committed,
                next.membership.clone(),
                next.introduced_at.unwrap(),
            )
        };
        assert_eq!(
            plan(&joint, &old, &final_config),
            Err(MembershipTransitionError::PendingMembership)
        );
        assert_eq!(
            plan(&joint, &joint, &final_config),
            Ok(final_config.clone())
        );
        // Joint is committed but final is merely effective: neither the batch
        // validator nor the planner may begin the following transition yet.
        assert_eq!(
            validate_membership_sequence(
                &old,
                [&joint, &final_config, &next_joint],
                joint.introduced_at
            ),
            Err(MembershipTransitionError::PredecessorNotCommitted)
        );
        assert_eq!(
            plan(&final_config, &joint, &next_joint),
            Err(MembershipTransitionError::PendingMembership)
        );
        assert_eq!(
            validate_membership_sequence(
                &old,
                [&joint, &final_config, &next_joint],
                final_config.introduced_at
            ),
            Ok(())
        );
        assert_eq!(
            plan(&final_config, &final_config, &next_joint),
            Ok(next_joint)
        );
    }

    #[test]
    fn membership_transition_rejects_illegal_voter_chains() {
        let old = transition_record(10, vec![vec![1, 2, 3]]);
        let direct = transition_record(11, vec![vec![2, 3, 4]]);
        assert_eq!(
            validate_membership_successor(&old, &direct, old.introduced_at),
            Err(MembershipTransitionError::DirectVoterChange)
        );
        let wrong_joint = transition_record(11, vec![vec![1, 2], vec![2, 3, 4]]);
        assert_eq!(
            validate_membership_successor(&old, &wrong_joint, old.introduced_at),
            Err(MembershipTransitionError::JointPredecessorMismatch)
        );
        let joint = transition_record(11, vec![vec![1, 2, 3], vec![2, 3, 4]]);
        let nested = transition_record(12, vec![vec![2, 3, 4], vec![3, 4, 5]]);
        assert_eq!(
            validate_membership_successor(&joint, &nested, joint.introduced_at),
            Err(MembershipTransitionError::NestedJoint)
        );
        let wrong_final = transition_record(12, vec![vec![1, 2, 3]]);
        assert_eq!(
            validate_membership_successor(&joint, &wrong_final, joint.introduced_at),
            Err(MembershipTransitionError::FinalVotersMismatch)
        );
        let final_config = transition_record(12, vec![vec![2, 3, 4]]);
        let next_joint = transition_record(13, vec![vec![2, 3, 4], vec![3, 4, 5]]);
        assert_eq!(
            validate_membership_sequence(
                &old,
                [&joint, &final_config, &next_joint],
                joint.introduced_at
            ),
            Err(MembershipTransitionError::PredecessorNotCommitted)
        );
        assert_eq!(
            validate_membership_sequence(
                &old,
                [&joint, &final_config, &next_joint],
                final_config.introduced_at
            ),
            Ok(())
        );
    }

    #[test]
    fn membership_transition_checks_genesis_positions_and_content() {
        let genesis = transition_record(0, vec![vec![1]]);
        assert_eq!(validate_membership_genesis(&genesis), Ok(()));
        assert_eq!(
            validate_membership_genesis(&transition_record(1, vec![vec![1]])),
            Err(MembershipTransitionError::InvalidGenesis)
        );
        assert_eq!(
            validate_membership_genesis(&transition_record(0, vec![vec![1], vec![2]])),
            Err(MembershipTransitionError::InvalidGenesis)
        );
        let mut missing = genesis;
        missing.introduced_at = None;
        assert_eq!(
            validate_membership_genesis(&missing),
            Err(MembershipTransitionError::MissingPosition)
        );
        let old = transition_record(10, vec![vec![1, 2, 3]]);
        let mut altered = old.clone();
        altered
            .membership
            .nodes
            .insert(NodeId::from(5), "changed".into());
        assert_eq!(
            validate_membership_successor(&old, &altered, old.introduced_at),
            Err(MembershipTransitionError::ConflictingIdentity)
        );
        assert_eq!(
            validate_membership_sequence(&old, [&old], old.introduced_at),
            Err(MembershipTransitionError::NonAdvancingPosition)
        );
        let earlier = transition_record(9, vec![vec![1, 2, 3]]);
        assert_eq!(
            validate_membership_successor(&old, &earlier, old.introduced_at),
            Err(MembershipTransitionError::NonAdvancingPosition)
        );
        altered.introduced_at = Some(LogIdentity {
            index: LogIndex(11),
            ..old.introduced_at.unwrap()
        });
        assert_eq!(
            validate_membership_successor(&old, &altered, old.introduced_at),
            Ok(()),
            "node-only change preserves voters"
        );
        let mut invalid = altered.clone();
        invalid.introduced_at.as_mut().unwrap().term = Term(2);
        assert_eq!(
            validate_membership_successor(&old, &invalid, old.introduced_at),
            Err(MembershipTransitionError::InvalidPosition)
        );
        invalid.introduced_at = Some(LogIdentity {
            leader: NodeId::from(2),
            ..altered.introduced_at.unwrap()
        });
        assert_eq!(
            validate_membership_successor(&old, &invalid, old.introduced_at),
            Err(MembershipTransitionError::ConflictingIdentity)
        );
    }

    #[test]
    fn membership_transition_planner_checks_commit_phases_and_full_identity_aba() {
        let old = transition_record(10, vec![vec![1, 2, 3]]);
        let joint = transition_record(11, vec![vec![1, 2, 3], vec![2, 3, 4]]);
        let final_config = transition_record(12, vec![vec![2, 3, 4]]);
        let plan = |effective: &StoredMembership,
                    committed: &StoredMembership,
                    expected: &StoredMembership,
                    next: &StoredMembership| {
            plan_membership_successor(
                effective,
                committed,
                expected,
                committed,
                next.membership.clone(),
                next.introduced_at.unwrap(),
            )
        };
        assert_eq!(plan(&old, &old, &old, &joint), Ok(joint.clone()));
        assert_eq!(
            plan(&joint, &old, &joint, &final_config),
            Err(MembershipTransitionError::PendingMembership)
        );
        assert_eq!(
            plan(&joint, &joint, &joint, &final_config),
            Ok(final_config.clone())
        );
        let next_joint = transition_record(13, vec![vec![2, 3, 4], vec![3, 4, 5]]);
        assert_eq!(
            plan(&final_config, &joint, &final_config, &next_joint),
            Err(MembershipTransitionError::PendingMembership)
        );
        assert_eq!(
            plan(&final_config, &final_config, &final_config, &next_joint),
            Ok(next_joint)
        );
        let same_voters_new_epoch = transition_record(20, vec![vec![1, 2, 3]]);
        assert_eq!(
            plan(&same_voters_new_epoch, &same_voters_new_epoch, &old, &joint),
            Err(MembershipTransitionError::StalePrecondition)
        );
        assert_eq!(
            plan_membership_successor(
                &same_voters_new_epoch,
                &same_voters_new_epoch,
                &same_voters_new_epoch,
                &old,
                joint.membership.clone(),
                joint.introduced_at.unwrap()
            ),
            Err(MembershipTransitionError::StalePrecondition),
            "committed expectation must also compare the full identity"
        );
        let mut changed_content = old.clone();
        changed_content
            .membership
            .nodes
            .insert(NodeId::from(5), "replacement".into());
        assert_eq!(
            plan(&old, &changed_content, &old, &joint),
            Err(MembershipTransitionError::ConflictingIdentity)
        );
    }

    fn read_barrier_fixture(
        configurations: Vec<Vec<u64>>,
    ) -> (LeaderProgress, HardState, StoredMembership, LogIdentity) {
        let membership = Membership::new(
            configurations
                .into_iter()
                .map(|voters| voters.into_iter().map(NodeId::from).collect())
                .collect(),
            (1..=6)
                .map(|id| (NodeId::from(id), String::new()))
                .collect(),
        )
        .unwrap();
        let committed = LogIdentity {
            term: Term(3),
            leader: NodeId::from(1),
            index: LogIndex(10),
        };
        let leader = LeaderProgress::after_durable_initialization(membership.clone(), committed);
        let state = HardState {
            term: committed.term,
            voted_for: Some(committed.leader),
        };
        let stored = StoredMembership {
            introduced_at: None,
            membership,
        };
        (leader, state, stored, committed)
    }

    // Fixed xorshift ordering is reproducible without clocks, OS randomness, or
    // dependencies. Each schedule has a finite explicit message budget.
    fn fault_schedule_shuffle<T>(messages: &mut [T], seed: &mut u64) {
        messages.sort_by_cached_key(|_| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        });
    }

    fn fault_schedule_elect(
        membership: &StoredMembership,
        state: HardState,
        previous: LogIdentity,
        minority: &[u64],
        seed: &mut u64,
    ) -> LeaderProgress {
        let request = RequestVote {
            term: state.term,
            candidate: previous.leader,
            last_log: Some(previous),
        };
        let mut campaign = Some(Campaign::new(request, membership.membership.clone()));
        let voters: BTreeSet<_> = membership
            .membership
            .configurations()
            .iter()
            .flatten()
            .copied()
            .collect();
        let mut messages = Vec::new();
        for peer in &voters {
            let available = minority.iter().any(|id| NodeId::from(*id) == *peer);
            // Old grants from the unreachable majority must not manufacture an
            // election. Reachable votes are duplicated and independently ordered.
            for _ in 0..3 {
                messages.push((
                    *peer,
                    VoteResponse {
                        term: if available {
                            state.term
                        } else {
                            Term(state.term.0 - 1)
                        },
                        granted: true,
                    },
                ));
            }
        }
        fault_schedule_shuffle(&mut messages, seed);
        for (peer, response) in messages {
            let decision = Campaign::observe(
                &mut campaign,
                state,
                Some(&membership.membership),
                state.term,
                peer,
                response,
            );
            assert!(!decision.quorum, "minority election at seed {seed}");
            assert_eq!(decision.persist, None);
            assert_eq!(
                campaign
                    .as_ref()
                    .unwrap()
                    .leader_initialization(state, &membership.membership, Some(previous))
                    .unwrap(),
                None
            );
        }
        let mut healed: Vec<_> = voters.into_iter().collect();
        fault_schedule_shuffle(&mut healed, seed);
        let mut quorum = false;
        for peer in healed {
            quorum = Campaign::observe(
                &mut campaign,
                state,
                Some(&membership.membership),
                state.term,
                peer,
                VoteResponse {
                    term: state.term,
                    granted: true,
                },
            )
            .quorum;
        }
        assert!(quorum, "healed election must progress");
        let initialization = campaign
            .unwrap()
            .leader_initialization(state, &membership.membership, Some(previous))
            .unwrap()
            .unwrap();
        assert_eq!(initialization.entries.len(), 1);
        assert_eq!(initialization.entries[0].payload, EntryPayload::NoOp);
        assert_eq!(
            initialization.entries[0].identity.index,
            previous.index.successor().unwrap()
        );
        // Model an adapter's successful durable initialization, not an election
        // vote being treated as durable remote replication.
        LeaderProgress::after_durable_initialization(
            membership.membership.clone(),
            initialization.entries[0].identity,
        )
    }

    fn fault_schedule_replicate(
        leader: &mut LeaderProgress,
        minority: &[u64],
        entry: LogIdentity,
        seed: &mut u64,
    ) {
        let older = LogIdentity {
            index: LogIndex(entry.index.0 - 1),
            term: Term(entry.term.0 - 1),
            ..entry
        };
        assert_eq!(leader.quorum_index(), None, "votes are not replication");
        let mut messages = Vec::new();
        for id in minority {
            for _ in 0..3 {
                messages.push((NodeId::from(*id), Some(entry)));
                messages.push((NodeId::from(*id), Some(older)));
            }
        }
        fault_schedule_shuffle(&mut messages, seed);
        for (peer, matched) in messages {
            leader.acknowledge_probe(peer, matched);
            assert_eq!(
                leader.quorum_index(),
                None,
                "minority cannot produce a commit candidate at seed {seed}"
            );
        }
        let mut healed: Vec<_> = leader.membership.nodes().keys().copied().collect();
        fault_schedule_shuffle(&mut healed, seed);
        for peer in healed {
            leader.acknowledge_probe(peer, Some(entry));
            leader.acknowledge_probe(peer, Some(older));
            assert_eq!(
                leader.matched[&peer],
                Some(entry),
                "delayed older success must not regress match"
            );
        }
        assert_eq!(leader.quorum_index(), Some(entry.index));
        // This is a quorum candidate, not a simulated durable commit. The real
        // store owns current-term validation and persistence of that decision.
        assert_eq!(entry.term, leader.term);
    }

    fn fault_schedule_reads(
        leader: &LeaderProgress,
        state: HardState,
        membership: &StoredMembership,
        entry: LogIdentity,
        minority: &[u64],
        seed: &mut u64,
    ) {
        let mut barrier = ReadBarrier::default();
        let old = barrier
            .begin(leader, state, membership, Some(entry))
            .unwrap()
            .unwrap();
        let context = barrier
            .begin(leader, state, membership, Some(entry))
            .unwrap()
            .unwrap();
        let mut messages = Vec::new();
        for peer in membership.membership.nodes().keys() {
            for _ in 0..3 {
                messages.push((
                    *peer,
                    ReadConfirmation {
                        context: old,
                        term: state.term,
                        accepted: true,
                    },
                ));
                let available = minority.iter().any(|id| NodeId::from(*id) == *peer);
                messages.push((
                    *peer,
                    ReadConfirmation {
                        context,
                        term: state.term,
                        accepted: available,
                    },
                ));
                messages.push((
                    *peer,
                    ReadConfirmation {
                        context,
                        term: Term(state.term.0 - 1),
                        accepted: true,
                    },
                ));
            }
        }
        fault_schedule_shuffle(&mut messages, seed);
        for (peer, response) in messages {
            assert_eq!(
                barrier.observe(Some(leader), state, membership, peer, response),
                None
            );
            assert_eq!(
                barrier.complete(Some(leader), state, membership, Some(entry), Some(entry)),
                None,
                "minority read despite fully applied committed state at seed {seed}"
            );
        }
        let mut healed: Vec<_> = membership.membership.nodes().keys().copied().collect();
        fault_schedule_shuffle(&mut healed, seed);
        let behind = LogIdentity {
            index: LogIndex(entry.index.0 - 1),
            ..entry
        };
        for peer in healed {
            barrier.observe(
                Some(leader),
                state,
                membership,
                peer,
                ReadConfirmation {
                    context,
                    term: state.term,
                    accepted: true,
                },
            );
            assert_eq!(
                barrier.complete(Some(leader), state, membership, Some(entry), Some(behind)),
                None,
                "quorum cannot bypass application lag"
            );
        }
        assert_eq!(
            barrier.complete(Some(leader), state, membership, Some(entry), Some(entry)),
            Some(entry)
        );
        assert_eq!(
            barrier.complete(Some(leader), state, membership, Some(entry), Some(entry)),
            None
        );
    }

    #[test]
    fn seeded_fault_schedule_minority_election_replication_and_reads() {
        // Joint case deliberately reaches an old-set majority (1,2) but not
        // the new-set majority (3,4,5): union-majority counting would fail here.
        let cases = [
            (vec![vec![1, 2, 3]], vec![1]),
            (vec![vec![1, 2, 3, 4, 5]], vec![1, 2]),
            (vec![vec![1, 2, 3], vec![3, 4, 5]], vec![1, 2]),
        ];
        for initial_seed in [1, 0x0015_5eed, 0xdead_beef, 0x0123_4567_89ab_cdef] {
            for (configurations, minority) in &cases {
                let mut seed = initial_seed;
                let (_, state, membership, previous) = read_barrier_fixture(configurations.clone());
                let previous = LogIdentity {
                    term: Term(2),
                    ..previous
                };
                let mut leader =
                    fault_schedule_elect(&membership, state, previous, minority, &mut seed);
                let entry = leader.matched[&previous.leader].unwrap();
                fault_schedule_replicate(&mut leader, minority, entry, &mut seed);
                // Supply the current-term watermark only after replication has
                // produced a quorum candidate. Persistence itself is not tested.
                fault_schedule_reads(&leader, state, &membership, entry, minority, &mut seed);
            }
        }
    }

    fn fault_schedule_invalidate(
        barrier: &mut ReadBarrier,
        leader: &LeaderProgress,
        state: HardState,
        membership: &StoredMembership,
        entry: LogIdentity,
        context: ReadContext,
        mode: u8,
    ) {
        match mode {
            0 => {
                let higher = state.term.successor().unwrap();
                let request = RequestVote {
                    term: state.term,
                    candidate: entry.leader,
                    last_log: Some(entry),
                };
                let mut campaign = Some(Campaign::new(request, membership.membership.clone()));
                let decision = Campaign::observe(
                    &mut campaign,
                    state,
                    Some(&membership.membership),
                    state.term,
                    NodeId::from(2),
                    VoteResponse {
                        term: higher,
                        granted: false,
                    },
                );
                assert_eq!(
                    decision.persist,
                    Some(HardState {
                        term: higher,
                        voted_for: None
                    })
                );
                assert!(campaign.is_none());
                assert_eq!(
                    barrier.observe(
                        Some(leader),
                        state,
                        membership,
                        NodeId::from(2),
                        ReadConfirmation {
                            context,
                            term: higher,
                            accepted: false
                        },
                    ),
                    Some(higher)
                );
            }
            1 => {
                let mut changed = membership.clone();
                changed.introduced_at = Some(entry);
                assert_eq!(
                    barrier.complete(Some(leader), state, &changed, Some(entry), Some(entry)),
                    None
                );
            }
            _ => {
                assert_eq!(
                    barrier.complete(None, state, membership, Some(entry), Some(entry)),
                    None
                );
            }
        }
    }

    #[test]
    fn seeded_fault_schedule_authority_changes_cannot_revive_reads() {
        for initial_seed in [1, 0x0015_5eed, 0xdead_beef, 0x0123_4567_89ab_cdef] {
            for configurations in [
                vec![vec![1, 2, 3]],
                vec![vec![1, 2, 3, 4, 5]],
                vec![vec![1, 2, 3], vec![3, 4, 5]],
            ] {
                let mut seed = initial_seed;
                let (_, state, membership, previous) = read_barrier_fixture(configurations);
                let previous = LogIdentity {
                    term: Term(2),
                    ..previous
                };
                let mut leader =
                    fault_schedule_elect(&membership, state, previous, &[1], &mut seed);
                let entry = leader.matched[&previous.leader].unwrap();
                fault_schedule_replicate(&mut leader, &[1], entry, &mut seed);
                let mut barrier = ReadBarrier::default();
                for mode in 0..3 {
                    let context = barrier
                        .begin(&leader, state, &membership, Some(entry))
                        .unwrap()
                        .unwrap();
                    let mut delayed: Vec<_> = membership
                        .membership
                        .nodes()
                        .keys()
                        .map(|peer| {
                            (
                                *peer,
                                ReadConfirmation {
                                    context,
                                    term: state.term,
                                    accepted: true,
                                },
                            )
                        })
                        .collect();
                    fault_schedule_shuffle(&mut delayed, &mut seed);
                    for (peer, response) in &delayed {
                        barrier.observe(Some(&leader), state, &membership, *peer, *response);
                    }
                    // Quorum is already confirmed but application is behind.
                    let round = barrier.pending.as_ref().unwrap();
                    assert!(round.membership.membership.has_quorum(&round.acknowledged));
                    assert_eq!(
                        barrier.complete(Some(&leader), state, &membership, Some(entry), None),
                        None
                    );
                    fault_schedule_invalidate(
                        &mut barrier,
                        &leader,
                        state,
                        &membership,
                        entry,
                        context,
                        mode,
                    );
                    // Even restoring the old caller inputs and delivering every
                    // delayed/duplicate reply cannot resurrect cancelled authority.
                    for (peer, response) in delayed.into_iter().rev() {
                        barrier.observe(Some(&leader), state, &membership, peer, response);
                        barrier.observe(Some(&leader), state, &membership, peer, response);
                        assert_eq!(
                            barrier.complete(
                                Some(&leader),
                                state,
                                &membership,
                                Some(entry),
                                Some(entry)
                            ),
                            None,
                            "cancelled read revived: seed {initial_seed}, mode {mode}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn read_barrier_requires_current_term_committed_leader_entry() {
        let (leader, state, membership, committed) = read_barrier_fixture(vec![vec![1]]);
        let mut barrier = ReadBarrier::default();
        for position in [
            None,
            Some(LogIdentity {
                term: Term(2),
                ..committed
            }),
            Some(LogIdentity {
                leader: NodeId::from(2),
                ..committed
            }),
            Some(LogIdentity {
                index: LogIndex(11),
                ..committed
            }),
        ] {
            assert_eq!(
                barrier.begin(&leader, state, &membership, position),
                Ok(None)
            );
        }
        assert_eq!(
            barrier.begin(
                &leader,
                HardState {
                    term: Term(4),
                    ..state
                },
                &membership,
                Some(committed)
            ),
            Ok(None)
        );
        assert!(
            barrier
                .begin(&leader, state, &membership, Some(committed))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            barrier.complete(Some(&leader), state, &membership, Some(committed), None),
            None
        );
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            Some(committed)
        );
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            None,
            "authority is consumed once"
        );
    }

    #[test]
    fn read_barrier_waits_for_joint_quorum_and_applied_watermark() {
        let (mut leader, state, membership, committed) =
            read_barrier_fixture(vec![vec![1, 2, 3], vec![3, 4, 5]]);
        for peer in 2..=6 {
            leader.acknowledge_probe(NodeId::from(peer), Some(committed));
        }
        let mut barrier = ReadBarrier::default();
        let context = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        let response = ReadConfirmation {
            context,
            term: state.term,
            accepted: true,
        };
        for peer in [2, 2, 6, 99] {
            assert_eq!(
                barrier.observe(
                    Some(&leader),
                    state,
                    &membership,
                    NodeId::from(peer),
                    response
                ),
                None
            );
        }
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            None,
            "replication progress, learners, and duplicate replies do not confirm reads"
        );
        barrier.observe(Some(&leader), state, &membership, NodeId::from(3), response);
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            None
        );
        barrier.observe(Some(&leader), state, &membership, NodeId::from(4), response);
        let behind = LogIdentity {
            index: LogIndex(9),
            ..committed
        };
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(behind)
            ),
            None
        );
        let later = LogIdentity {
            index: LogIndex(11),
            ..committed
        };
        assert_eq!(
            barrier.complete(Some(&leader), state, &membership, Some(later), Some(later)),
            Some(committed),
            "application may pass the captured watermark"
        );
    }

    #[test]
    fn read_barrier_rejects_stale_foreign_and_rejected_confirmations() {
        let (leader, state, membership, committed) = read_barrier_fixture(vec![vec![1, 2, 3]]);
        let mut barrier = ReadBarrier::default();
        let old = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        let context = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        assert_ne!(old, context);
        let response = ReadConfirmation {
            context,
            term: state.term,
            accepted: true,
        };
        for invalid in [
            ReadConfirmation {
                context: old,
                ..response
            },
            ReadConfirmation {
                context: ReadContext {
                    term: Term(2),
                    ..context
                },
                ..response
            },
            ReadConfirmation {
                context: ReadContext {
                    leader: NodeId::from(2),
                    ..context
                },
                ..response
            },
            ReadConfirmation {
                term: Term(2),
                ..response
            },
            ReadConfirmation {
                accepted: false,
                ..response
            },
        ] {
            barrier.observe(Some(&leader), state, &membership, NodeId::from(2), invalid);
            assert_eq!(
                barrier.complete(
                    Some(&leader),
                    state,
                    &membership,
                    Some(committed),
                    Some(committed)
                ),
                None
            );
        }
        barrier.observe(Some(&leader), state, &membership, NodeId::from(2), response);
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            Some(committed)
        );
    }

    #[test]
    fn read_barrier_invalidates_on_term_membership_or_leadership_change() {
        let (leader, state, membership, committed) = read_barrier_fixture(vec![vec![1, 2, 3]]);
        let mut barrier = ReadBarrier::default();
        let context = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        let response = ReadConfirmation {
            context,
            term: state.term,
            accepted: true,
        };
        barrier.observe(Some(&leader), state, &membership, NodeId::from(2), response);
        let mut changed = membership.clone();
        changed.introduced_at = Some(committed);
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &changed,
                Some(committed),
                Some(committed)
            ),
            None
        );
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            None
        );

        for newer_term in [false, true] {
            barrier
                .begin(&leader, state, &membership, Some(committed))
                .unwrap();
            let next_state = HardState {
                term: Term(4),
                voted_for: None,
            };
            assert_eq!(
                barrier.complete(
                    newer_term.then_some(&leader),
                    if newer_term { next_state } else { state },
                    &membership,
                    Some(committed),
                    Some(committed),
                ),
                None
            );
            assert!(barrier.pending.is_none());
        }
        let context = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        let higher = ReadConfirmation {
            context: ReadContext {
                sequence: 0,
                ..context
            },
            term: Term(4),
            accepted: true,
        };
        assert_eq!(
            barrier.observe(Some(&leader), state, &membership, NodeId::from(6), higher),
            None
        );
        assert!(
            barrier.pending.is_some(),
            "learner cannot change term authority"
        );
        assert_eq!(
            barrier.observe(Some(&leader), state, &membership, NodeId::from(2), higher),
            Some(Term(4))
        );
        assert!(
            barrier.pending.is_none(),
            "even stale higher-term replies cancel reads"
        );
    }

    #[test]
    fn read_barrier_does_not_mix_effective_configurations() {
        let (leader, state, membership, committed) = read_barrier_fixture(vec![vec![1, 2, 3]]);
        let (changed_leader, _, changed_membership, _) = read_barrier_fixture(vec![vec![1, 3, 4]]);
        let mut barrier = ReadBarrier::default();
        assert_eq!(
            barrier.begin(&leader, state, &changed_membership, Some(committed)),
            Ok(None)
        );
        let context = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        let response = ReadConfirmation {
            context,
            term: state.term,
            accepted: true,
        };
        barrier.observe(Some(&leader), state, &membership, NodeId::from(2), response);
        barrier.observe(
            Some(&changed_leader),
            state,
            &changed_membership,
            NodeId::from(2),
            response,
        );
        assert!(
            barrier.pending.is_none(),
            "a removed voter's reply still observes the configuration change"
        );
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            None,
            "restoring the configuration cannot revive the cancelled round"
        );
        let next = barrier
            .begin(&changed_leader, state, &changed_membership, Some(committed))
            .unwrap()
            .unwrap();
        barrier.observe(
            Some(&changed_leader),
            state,
            &changed_membership,
            NodeId::from(3),
            response,
        );
        assert_eq!(
            barrier.complete(
                Some(&changed_leader),
                state,
                &changed_membership,
                Some(committed),
                Some(committed)
            ),
            None
        );
        barrier.observe(
            Some(&changed_leader),
            state,
            &changed_membership,
            NodeId::from(3),
            ReadConfirmation {
                context: next,
                ..response
            },
        );
        assert_eq!(
            barrier.complete(
                Some(&changed_leader),
                state,
                &changed_membership,
                Some(committed),
                Some(committed)
            ),
            Some(committed)
        );
    }

    #[test]
    fn read_barrier_cancellation_and_exhaustion_never_reuse_context() {
        let (leader, state, membership, committed) = read_barrier_fixture(vec![vec![1]]);
        let mut barrier = ReadBarrier::default();
        let old = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        barrier.cancel();
        assert_eq!(
            barrier.complete(
                Some(&leader),
                state,
                &membership,
                Some(committed),
                Some(committed)
            ),
            None
        );
        let next = barrier
            .begin(&leader, state, &membership, Some(committed))
            .unwrap()
            .unwrap();
        assert_ne!(old, next);
        barrier.next_sequence = Some(u64::MAX);
        assert_eq!(
            barrier
                .begin(&leader, state, &membership, Some(committed))
                .unwrap()
                .unwrap()
                .sequence,
            u64::MAX
        );
        assert_eq!(
            barrier.begin(&leader, state, &membership, Some(committed)),
            Err(ReadSequenceExhausted)
        );
        assert!(barrier.pending.is_none());
    }

    #[test]
    fn read_barrier_rejects_conflicting_or_regressed_storage_watermarks() {
        let (leader, state, membership, committed) = read_barrier_fixture(vec![vec![1]]);
        let mut barrier = ReadBarrier::default();
        let conflicting = LogIdentity {
            leader: NodeId::from(2),
            ..committed
        };
        let earlier = LogIdentity {
            index: LogIndex(9),
            ..committed
        };
        let later = LogIdentity {
            index: LogIndex(11),
            ..committed
        };
        for (commit, applied) in [
            (None, None),
            (Some(earlier), Some(earlier)),
            (Some(conflicting), Some(conflicting)),
            (Some(committed), Some(conflicting)),
            (Some(committed), Some(later)),
        ] {
            barrier
                .begin(&leader, state, &membership, Some(committed))
                .unwrap();
            assert_eq!(
                barrier.complete(Some(&leader), state, &membership, commit, applied),
                None
            );
            assert!(barrier.pending.is_none());
        }
    }

    #[test]
    fn position_prefix_requires_coherent_coverage() {
        let id = LogIdentity {
            term: Term(2),
            index: LogIndex(0),
            leader: NodeId::from(1),
        };
        assert_eq!(validate_position_prefix(None, None), Ok(()));
        assert_eq!(validate_position_prefix(Some(id), Some(id)), Ok(()));
        assert_eq!(
            validate_position_prefix(Some(id), None),
            Err(PositionError::MissingBoundary)
        );
        let later = LogIdentity {
            index: LogIndex(1),
            ..id
        };
        assert_eq!(validate_position_prefix(Some(id), Some(later)), Ok(()));
        assert_eq!(
            validate_position_prefix(Some(later), Some(id)),
            Err(PositionError::BeyondBoundary)
        );
        assert_eq!(
            validate_position_prefix(
                Some(id),
                Some(LogIdentity {
                    leader: NodeId::from(2),
                    ..id
                })
            ),
            Err(PositionError::ConflictingIdentity)
        );
        assert_eq!(
            validate_position_prefix(
                Some(id),
                Some(LogIdentity {
                    term: Term(1),
                    ..later
                })
            ),
            Err(PositionError::DecreasingTerm)
        );
    }

    #[test]
    fn exhaustion_never_wraps() {
        assert_eq!(Term(u64::MAX).successor(), Err(IdentityExhaustion::Term));
        assert_eq!(
            LogIndex(u64::MAX).successor(),
            Err(IdentityExhaustion::LogIndex)
        );
        assert_eq!(Term(0).successor(), Ok(Term(1)));
        assert_eq!(LogIndex(0).successor(), Ok(LogIndex(1)));
    }

    #[test]
    fn freshness_and_matching_are_distinct() {
        let first = LogIdentity {
            term: Term(0),
            leader: NodeId::from(1),
            index: LogIndex(0),
        };
        let other = LogIdentity {
            leader: NodeId::from(2),
            ..first
        };
        assert_ne!(first, other);
        assert_eq!(
            LogIdentity::compare_freshness(Some(first), Some(other)),
            Ordering::Equal
        );
        assert_eq!(
            LogIdentity::compare_freshness(Some(first), None),
            Ordering::Greater
        );
        assert_eq!(
            LogIdentity::compare_freshness(None, Some(first)),
            Ordering::Less
        );
        let newer_term = LogIdentity {
            term: Term(1),
            ..first
        };
        let longer_old = LogIdentity {
            index: LogIndex(u64::MAX),
            ..first
        };
        assert_eq!(
            LogIdentity::compare_freshness(Some(newer_term), Some(longer_old)),
            Ordering::Greater
        );
        assert_eq!(
            LogIdentity::compare_freshness(Some(longer_old), Some(first)),
            Ordering::Greater
        );
    }
}
