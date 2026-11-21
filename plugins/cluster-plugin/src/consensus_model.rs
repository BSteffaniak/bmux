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
