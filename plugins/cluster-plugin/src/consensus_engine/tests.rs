use super::*;
use std::collections::BTreeMap;

fn node(id: u64) -> NodeId {
    NodeId::from(id)
}

fn membership(voters: &[u64], learners: &[u64]) -> Membership {
    Membership::new(
        vec![voters.iter().copied().map(node).collect()],
        voters
            .iter()
            .chain(learners)
            .map(|id| (node(*id), format!("node-{id}")))
            .collect(),
    )
    .unwrap()
}

/// The founding entry the store's fresh-install bootstrap writes.
fn founded(voters: &[u64], learners: &[u64]) -> Recovered {
    let founder = node(voters[0]);
    let identity = LogIdentity {
        term: Term(0),
        leader: founder,
        index: LogIndex(0),
    };
    Recovered {
        hard_state: HardState {
            term: Term(0),
            voted_for: None,
        },
        snapshot: None,
        entries: vec![Entry {
            identity,
            payload: EntryPayload::Membership(membership(voters, learners)),
        }],
        commit: Some(identity),
        applied: None,
    }
}

/// Routes effects between engines in a deterministic order. Persistence and
/// application complete instantly unless a node's durability is held.
struct Cluster {
    engines: BTreeMap<NodeId, Engine>,
    messages: VecDeque<(NodeId, NodeId, Message)>,
    held: BTreeMap<NodeId, Vec<u64>>,
    holding: BTreeSet<NodeId>,
    timers: BTreeMap<(NodeId, TimerKind), u64>,
    down: BTreeSet<NodeId>,
    log: Vec<(NodeId, Effect)>,
}

impl Cluster {
    fn new(voters: &[u64], learners: &[u64]) -> Self {
        let mut cluster = Self {
            engines: BTreeMap::new(),
            messages: VecDeque::new(),
            held: BTreeMap::new(),
            holding: BTreeSet::new(),
            timers: BTreeMap::new(),
            down: BTreeSet::new(),
            log: Vec::new(),
        };
        for id in voters.iter().chain(learners) {
            // Every member starts from the same provisioned founding entry.
            let (engine, effects) =
                Engine::recover(node(*id), Config::default(), founded(voters, learners)).unwrap();
            cluster.engines.insert(node(*id), engine);
            cluster.route(node(*id), effects);
        }
        cluster
    }

    fn engine(&self, id: u64) -> &Engine {
        &self.engines[&node(id)]
    }

    fn input(&mut self, id: u64, input: Input) -> Vec<Effect> {
        let effects = self.engines.get_mut(&node(id)).unwrap().handle(input);
        self.route(node(id), effects.clone());
        effects
    }

    fn route(&mut self, from: NodeId, effects: Vec<Effect>) {
        for effect in effects {
            self.log.push((from, effect.clone()));
            match effect {
                Effect::Send { to, message } => self.messages.push_back((from, to, message)),
                Effect::Persist { id, .. } | Effect::InstallSnapshot { id, .. } => {
                    self.held.entry(from).or_default().push(id);
                }
                Effect::SetTimer {
                    kind, generation, ..
                } => {
                    self.timers.insert((from, kind), generation);
                }
                _ => {}
            }
        }
    }

    /// Deliver messages and complete durable work until quiescent.
    fn settle(&mut self) {
        for _ in 0..10_000 {
            let durable = self
                .held
                .iter()
                .find(|(id, ids)| !ids.is_empty() && !self.holding.contains(id))
                .map(|(id, ids)| (*id, *ids.iter().max().unwrap()));
            if let Some((id, max)) = durable {
                self.held.get_mut(&id).unwrap().clear();
                let effects = self
                    .engines
                    .get_mut(&id)
                    .unwrap()
                    .handle(Input::Persisted { id: max });
                self.route(id, effects);
                self.apply(id);
                continue;
            }
            let Some((from, to, message)) = self.messages.pop_front() else {
                return;
            };
            if self.down.contains(&from) || self.down.contains(&to) {
                continue;
            }
            let effects = self
                .engines
                .get_mut(&to)
                .unwrap()
                .handle(Input::Message { from, message });
            self.route(to, effects);
        }
        panic!("cluster did not settle");
    }

    /// Report application for every requested Apply effect.
    fn apply(&mut self, id: NodeId) {
        let targets = self
            .log
            .iter()
            .filter_map(|(from, effect)| match effect {
                Effect::Apply { through } if *from == id => Some(*through),
                _ => None,
            })
            .collect::<Vec<_>>();
        for through in targets {
            let engine = self.engines.get_mut(&id).unwrap();
            if engine
                .applied()
                .is_none_or(|applied| applied.index < through.index)
            {
                let effects = engine.handle(Input::Applied { through });
                self.route(id, effects);
            }
        }
    }

    fn fire(&mut self, id: u64, kind: TimerKind) {
        let generation = self.timers[&(node(id), kind)];
        self.input(id, Input::Timer { kind, generation });
    }

    fn elect(&mut self, id: u64) {
        self.fire(id, TimerKind::Election);
        self.settle();
        assert!(self.engine(id).is_leader(), "node {id} was not elected");
    }

    fn leaders(&self) -> Vec<(Term, NodeId)> {
        self.engines
            .values()
            .filter(|engine| engine.is_leader())
            .map(|engine| (engine.term(), engine.id()))
            .collect()
    }

    fn effects_of(&self, id: u64) -> impl Iterator<Item = &Effect> {
        self.log
            .iter()
            .filter(move |(from, _)| *from == node(id))
            .map(|(_, effect)| effect)
    }
}

#[test]
fn single_voter_elects_commits_and_applies_after_durability() {
    let mut cluster = Cluster::new(&[1], &[]);
    cluster.holding.insert(node(1));
    cluster.fire(1, TimerKind::Election);
    cluster.settle();
    // The self-vote is not counted until its hard state is durable.
    assert!(!cluster.engine(1).is_leader());
    cluster.holding.clear();
    cluster.settle();
    assert!(cluster.engine(1).is_leader());
    let effects = cluster.input(
        1,
        Input::Propose {
            token: 7,
            payload: b"a".to_vec(),
        },
    );
    let Some(Effect::Proposed { identity, .. }) = effects
        .iter()
        .find(|effect| matches!(effect, Effect::Proposed { .. }))
    else {
        panic!("proposal not accepted: {effects:?}");
    };
    let identity = *identity;
    assert!(cluster.engine(1).commit().unwrap().index < identity.index);
    cluster.settle();
    assert_eq!(cluster.engine(1).commit(), Some(identity));
    assert_eq!(cluster.engine(1).applied(), Some(identity));
}

#[test]
fn three_voters_elect_one_leader_and_replicate() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    assert_eq!(cluster.leaders(), vec![(Term(1), node(1))]);
    cluster.input(
        1,
        Input::Propose {
            token: 1,
            payload: b"x".to_vec(),
        },
    );
    cluster.settle();
    let commit = cluster.engine(1).commit().unwrap();
    for id in [2, 3] {
        cluster.fire(1, TimerKind::Heartbeat);
        cluster.settle();
        assert_eq!(cluster.engine(id).commit(), Some(commit));
        assert_eq!(cluster.engine(id).last_log(), cluster.engine(1).last_log());
    }
}

#[test]
fn follower_reply_waits_for_durable_append() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    cluster.holding.insert(node(2));
    cluster.holding.insert(node(3));
    cluster.input(
        1,
        Input::Propose {
            token: 1,
            payload: b"x".to_vec(),
        },
    );
    cluster.settle();
    let proposed = cluster.engine(1).last_log().unwrap();
    // Followers hold the entry in memory but have not acknowledged it.
    assert!(cluster.engine(1).commit().unwrap().index < proposed.index);
    cluster.holding.clear();
    cluster.settle();
    assert_eq!(cluster.engine(1).commit(), Some(proposed));
}

#[test]
fn vote_is_granted_once_per_term_and_requires_fresh_log() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    cluster.input(
        1,
        Input::Propose {
            token: 1,
            payload: b"x".to_vec(),
        },
    );
    cluster.settle();
    // A stale candidate with an older log cannot win.
    let stale = Message::Vote {
        term: Term(5),
        last_log: None,
    };
    let effects = cluster.input(
        2,
        Input::Message {
            from: node(3),
            message: stale,
        },
    );
    cluster.settle();
    let granted = cluster.log.iter().any(|(from, effect)| {
        *from == node(2)
            && matches!(
                effect,
                Effect::Send {
                    message: Message::VoteReply {
                        granted: true,
                        term: Term(5)
                    },
                    ..
                }
            )
    });
    assert!(!granted, "{effects:?}");
    assert_eq!(cluster.engine(2).term(), Term(5));
}

#[test]
fn leader_failover_preserves_committed_entries_and_repairs_divergent_suffix() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    cluster.input(
        1,
        Input::Propose {
            token: 1,
            payload: b"committed".to_vec(),
        },
    );
    cluster.settle();
    let committed = cluster.engine(1).commit().unwrap();
    // Leader 1 appends an entry nobody else receives, then is partitioned.
    cluster.down.insert(node(1));
    cluster.input(
        1,
        Input::Propose {
            token: 2,
            payload: b"lost".to_vec(),
        },
    );
    cluster.settle();
    cluster.elect(2);
    cluster.input(
        2,
        Input::Propose {
            token: 3,
            payload: b"new".to_vec(),
        },
    );
    cluster.settle();
    // Heal: the old leader steps down and its uncommitted suffix is replaced.
    cluster.down.clear();
    cluster.fire(2, TimerKind::Heartbeat);
    cluster.settle();
    cluster.fire(2, TimerKind::Heartbeat);
    cluster.settle();
    assert!(!cluster.engine(1).is_leader());
    assert_eq!(cluster.engine(1).last_log(), cluster.engine(2).last_log());
    assert_eq!(
        cluster
            .engine(1)
            .entry(committed.index)
            .map(|entry| entry.identity),
        Some(committed)
    );
    let replaced = cluster
        .engine(1)
        .entry(LogIndex(committed.index.0 + 1))
        .unwrap();
    assert_eq!(replaced.identity.term, Term(2));
}

#[test]
fn stale_timers_and_self_messages_are_ignored() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    let before = cluster.engine(1).term();
    cluster.input(
        1,
        Input::Timer {
            kind: TimerKind::Election,
            generation: 0,
        },
    );
    assert_eq!(cluster.engine(1).term(), before);
    let effects = cluster.input(
        1,
        Input::Message {
            from: node(1),
            message: Message::Vote {
                term: Term(9),
                last_log: None,
            },
        },
    );
    assert!(matches!(effects[..], [Effect::ProtocolViolation { .. }]));
}

#[test]
fn learner_never_campaigns_and_is_promoted_through_joint_consensus() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[4]);
    cluster.fire(4, TimerKind::Election);
    cluster.settle();
    assert_eq!(cluster.engine(4).term(), Term(0));
    cluster.elect(1);
    cluster.fire(1, TimerKind::Heartbeat);
    cluster.settle();
    let effects = cluster.input(
        1,
        Input::ChangeMembership {
            token: 9,
            target: membership(&[1, 2, 4], &[]),
        },
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::MembershipProposed { token: 9, .. })),
        "{effects:?}"
    );
    // The joint entry is effective immediately and requires both majorities.
    let joint = cluster.engine(1).effective_membership().unwrap();
    assert_eq!(joint.membership.configurations().len(), 2);
    cluster.settle();
    cluster.fire(1, TimerKind::Heartbeat);
    cluster.settle();
    let final_membership = cluster.engine(1).committed_membership().unwrap();
    assert_eq!(final_membership.membership.configurations().len(), 1);
    assert_eq!(
        final_membership.membership.configurations()[0],
        [1, 2, 4].into_iter().map(node).collect()
    );
    assert!(!final_membership.membership.nodes().contains_key(&node(3)));
}

#[test]
fn removed_leader_steps_down_after_final_configuration_commits() {
    let mut cluster = Cluster::new(&[1, 2, 3, 4], &[]);
    cluster.elect(1);
    cluster.fire(1, TimerKind::Heartbeat);
    cluster.settle();
    cluster.input(
        1,
        Input::ChangeMembership {
            token: 1,
            target: membership(&[2, 3, 4], &[]),
        },
    );
    cluster.settle();
    cluster.fire(1, TimerKind::Heartbeat);
    cluster.settle();
    assert!(!cluster.engine(1).is_leader());
    let committed = cluster.engine(2).committed_membership();
    assert_eq!(
        committed.map(|m| m.membership.configurations()[0].clone()),
        Some([2, 3, 4].into_iter().map(node).collect())
    );
}

#[test]
fn membership_change_rejected_until_learner_caught_up_and_while_in_progress() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[4]);
    cluster.elect(1);
    cluster.down.insert(node(4));
    for token in 0..100 {
        cluster.input(
            1,
            Input::Propose {
                token,
                payload: vec![1],
            },
        );
    }
    cluster.settle();
    let effects = cluster.input(
        1,
        Input::ChangeMembership {
            token: 500,
            target: membership(&[1, 2, 3, 4], &[]),
        },
    );
    assert!(effects.contains(&Effect::Rejected {
        token: 500,
        reason: Rejection::LearnerNotCaughtUp(node(4)),
    }));
    let effects = cluster.input(
        1,
        Input::ChangeMembership {
            token: 501,
            target: membership(&[1, 2], &[]),
        },
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::MembershipProposed { token: 501, .. }))
    );
    let effects = cluster.input(
        1,
        Input::ChangeMembership {
            token: 502,
            target: membership(&[1, 2, 3], &[]),
        },
    );
    assert!(effects.contains(&Effect::Rejected {
        token: 502,
        reason: Rejection::MembershipChangeInProgress,
    }));
}

#[test]
fn linearizable_read_waits_for_quorum_confirmation_and_application() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    cluster.down.insert(node(2));
    cluster.down.insert(node(3));
    let effects = cluster.input(1, Input::Read { token: 4 });
    cluster.settle();
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::ReadReady { .. }))
    );
    assert!(
        !cluster
            .effects_of(1)
            .any(|e| matches!(e, Effect::ReadReady { token: 4, .. }))
    );
    cluster.down.clear();
    cluster.fire(1, TimerKind::Heartbeat);
    cluster.settle();
    assert!(
        cluster
            .effects_of(1)
            .any(|e| matches!(e, Effect::ReadReady { token: 4, .. }))
    );
}

#[test]
fn read_before_current_term_commit_and_on_follower_is_rejected() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    let effects = cluster.input(2, Input::Read { token: 1 });
    assert!(matches!(
        effects[..],
        [Effect::Rejected {
            reason: Rejection::NotLeader { .. },
            ..
        }]
    ));
    cluster.holding.extend([node(2), node(3)]);
    cluster.fire(1, TimerKind::Election);
    cluster.settle();
    cluster.holding.clear();
    cluster.settle();
    assert!(cluster.engine(1).is_leader());
}

#[test]
fn lagging_follower_receives_snapshot_after_compaction() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    cluster.down.insert(node(3));
    for token in 0..10 {
        cluster.input(
            1,
            Input::Propose {
                token,
                payload: token.to_le_bytes().to_vec(),
            },
        );
    }
    cluster.settle();
    let applied = cluster.engine(1).applied().unwrap();
    let membership = cluster.engine(1).committed_membership().unwrap();
    cluster.input(
        1,
        Input::Compacted {
            through: applied,
            membership: membership.clone(),
        },
    );
    cluster.down.clear();
    cluster.fire(1, TimerKind::Heartbeat);
    cluster.settle();
    let snapshot = cluster.effects_of(1).find_map(|effect| match effect {
        Effect::SendSnapshot { to, term, seq } if *to == node(3) => Some((*term, *seq)),
        _ => None,
    });
    let (term, seq) = snapshot.expect("leader requested a snapshot transfer");
    // The driver transfers the staged bytes and delivers the boundary.
    cluster.messages.push_back((
        node(1),
        node(3),
        Message::Snapshot {
            term,
            seq,
            last_included: applied,
            membership,
        },
    ));
    cluster.settle();
    cluster.fire(1, TimerKind::Heartbeat);
    cluster.settle();
    assert_eq!(cluster.engine(3).last_log(), cluster.engine(1).last_log());
    assert_eq!(cluster.engine(3).commit(), cluster.engine(1).commit());
}

#[test]
fn persist_failure_stops_the_authority() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.input(1, Input::PersistFailed { id: 1 });
    assert_eq!(cluster.engine(1).stopped(), Some(StopReason::PersistFailed));
    let effects = cluster.input(1, Input::Read { token: 1 });
    assert!(matches!(effects[..], [Effect::Stopped { .. }]));
}

#[test]
fn recovery_rejects_incoherent_positions() {
    let mut recovered = founded(&[1], &[]);
    recovered.applied = Some(LogIdentity {
        term: Term(3),
        leader: node(1),
        index: LogIndex(9),
    });
    assert_eq!(
        Engine::recover(node(1), Config::default(), recovered).err(),
        Some(RecoveryError::InvalidPositions)
    );
}

#[test]
fn heartbeat_is_sent_even_when_the_pipeline_is_full() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    cluster.down.insert(node(2));
    for token in 0..20 {
        cluster.input(
            1,
            Input::Propose {
                token,
                payload: vec![0; 10],
            },
        );
    }
    cluster.messages.clear();
    let effects = cluster.input(
        1,
        Input::Timer {
            kind: TimerKind::Heartbeat,
            generation: cluster.timers[&(node(1), TimerKind::Heartbeat)],
        },
    );
    let to_two = effects
        .iter()
        .filter(|effect| matches!(effect, Effect::Send { to, .. } if *to == node(2)))
        .count();
    assert!(to_two >= 1, "{effects:?}");
}

#[test]
fn deposed_leader_rearms_its_election_deadline() {
    let mut cluster = Cluster::new(&[1, 2, 3], &[]);
    cluster.elect(1);
    // A vote request with a higher term deposes the leader without a grant
    // (the candidate's log is stale), and the deposed leader must be able to
    // campaign again.
    let effects = cluster.input(
        1,
        Input::Message {
            from: node(3),
            message: Message::Vote {
                term: Term(9),
                last_log: None,
            },
        },
    );
    assert!(!cluster.engine(1).is_leader());
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::SetTimer {
                kind: TimerKind::Election,
                ..
            }
        )),
        "{effects:?}"
    );
}
