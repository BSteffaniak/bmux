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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaderProgress {
    pub term: Term,
    pub membership: Membership,
    pub matched: BTreeMap<NodeId, Option<LogIdentity>>,
}

impl LeaderProgress {
    pub fn after_durable_initialization(membership: Membership, entry: LogIdentity) -> Self {
        let matched = membership
            .nodes()
            .keys()
            .map(|peer| (*peer, (*peer == entry.leader).then_some(entry)))
            .collect();
        Self {
            term: entry.term,
            membership,
            matched,
        }
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
