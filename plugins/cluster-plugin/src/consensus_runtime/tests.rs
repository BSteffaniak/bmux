//! The owned runtime over real stores, with an in-process transport that can
//! partition nodes.

use super::*;
use crate::consensus_driver::Reply;
use crate::consensus_engine::Message;
use crate::control_codec::FeatureActivationCommand;
use bmux_cluster_plugin_api::cluster_types::{
    CommandId, ControlCommand, ControlCommandRequest, WorkspaceId,
};
use std::sync::RwLock;
use tempfile::TempDir;

/// Routes engine messages between registered nodes; isolated nodes neither
/// send nor receive.
#[derive(Clone, Default)]
pub struct MemoryNetwork {
    nodes: Arc<RwLock<BTreeMap<NodeId, ConsensusNode>>>,
    isolated: Arc<RwLock<BTreeSet<NodeId>>>,
}

impl MemoryNetwork {
    pub fn register(&self, node: &ConsensusNode) {
        self.nodes
            .write()
            .unwrap()
            .insert(node.node_id(), node.clone());
    }

    pub fn unregister(&self, id: NodeId) {
        self.nodes.write().unwrap().remove(&id);
    }

    pub fn isolate(&self, id: NodeId) {
        self.isolated.write().unwrap().insert(id);
    }

    pub fn heal(&self, id: NodeId) {
        self.isolated.write().unwrap().remove(&id);
    }

    pub fn transport(&self, local: NodeId) -> impl FnOnce(MemberView) -> Arc<MemoryTransport> {
        let network = self.clone();
        move |_| Arc::new(MemoryTransport { local, network })
    }
}

pub struct MemoryTransport {
    local: NodeId,
    network: MemoryNetwork,
}

impl Transport for MemoryTransport {
    fn call(&self, to: NodeId, message: Message) -> Reply {
        let local = self.local;
        let network = self.network.clone();
        Box::pin(async move {
            let blocked = {
                let isolated = network.isolated.read().unwrap();
                isolated.contains(&local) || isolated.contains(&to)
            };
            if blocked {
                return None;
            }
            let node = network.nodes.read().unwrap().get(&to).cloned()?;
            let driver = node.driver().ok()?;
            tokio::time::timeout(Duration::from_secs(2), driver.request(local, message))
                .await
                .ok()?
        })
    }

    fn send_snapshot(&self, _: NodeId, _: crate::consensus_model::Term, _: u64) -> Reply {
        Box::pin(async { None })
    }
}

pub fn endpoint(id: NodeId) -> String {
    format!("memory://{id}")
}

pub async fn start_node(
    root: &Path,
    id: NodeId,
    network: &MemoryNetwork,
    founder: bool,
) -> ConsensusNode {
    let provisioning = if founder {
        Provisioning::Founder {
            endpoint: endpoint(id),
        }
    } else {
        Provisioning::Joiner
    };
    let node = ConsensusNode::start(
        root,
        "cluster-test",
        id,
        provisioning,
        &PreflightAdmission::default(),
        network.transport(id),
    )
    .await
    .unwrap();
    network.register(&node);
    node
}

/// Wait until one of `nodes` leads with a quorum-confirmed read.
pub async fn wait_for_leader(nodes: &[&ConsensusNode]) -> NodeId {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            for node in nodes {
                if node.is_leader() && node.linearizable_control_state().await.is_ok() {
                    return node.node_id();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cluster should elect a leader")
}

pub fn command(id: u128, workspace: u128) -> ControlCommand {
    ControlCommand {
        schema_version: 1,
        principal_id: "principal:test".to_string(),
        command_id: CommandId {
            value: uuid::Uuid::from_u128(id),
        },
        issued_at_unix_ms: u64::try_from(id).unwrap(),
        request: ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId {
                value: uuid::Uuid::from_u128(workspace),
            },
            name: Some(format!("workspace-{workspace}")),
        },
    }
}

async fn wait_for_revision(node: &ConsensusNode, revision: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while node.control_revision() < revision {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("revision should replicate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founder_commits_dedups_and_recovers_after_restart() {
    let root = TempDir::new().unwrap();
    let network = MemoryNetwork::default();
    let id = NodeId::from(1);
    let node = start_node(root.path(), id, &network, true).await;
    assert_eq!(wait_for_leader(&[&node]).await, id);
    assert!(node.is_initialized());
    assert_eq!(node.committed_voter_ids(), BTreeSet::from([id]));
    let accepted = node.mutate(command(1, 1)).await.unwrap();
    let retried = node.mutate(command(1, 1)).await.unwrap();
    assert_eq!(
        accepted, retried,
        "a retried CommandId returns its response"
    );
    assert_eq!(node.persisted_control_state().revision, 1);
    let (term, first) = node.next_lease();
    assert_eq!(term, node.current_term());
    assert!(node.next_lease().1 > first);
    let epoch = node
        .read_linearizable_view()
        .await
        .unwrap()
        .membership_epoch;
    assert!(
        epoch.is_some(),
        "linearizable reads carry the membership epoch"
    );
    node.shutdown().await.unwrap();
    network.unregister(id);

    // Shutdown released durable ownership: the same store reopens.
    let node = start_node(root.path(), id, &network, true).await;
    assert_eq!(node.persisted_control_state().revision, 1);
    wait_for_leader(&[&node]).await;
    assert_eq!(node.mutate(command(1, 1)).await.unwrap(), accepted);
    node.mutate(command(2, 2)).await.unwrap();
    assert_eq!(node.linearizable_control_state().await.unwrap().revision, 2);
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_submission_requires_activation() {
    let root = TempDir::new().unwrap();
    let network = MemoryNetwork::default();
    let identity = crate::membership::NodeIdentity::new_for_test(93);
    let cluster: crate::membership::ClusterId = "cluster:00000000-0000-0000-0000-000000000093"
        .parse()
        .unwrap();
    let node = start_node(root.path(), *identity.node_id(), &network, true).await;
    wait_for_leader(&[&node]).await;
    assert!(matches!(
        node.mutate_bounded(command(1, 0)).await,
        Err(ControlServiceError::Rejected { .. })
    ));
    assert_eq!(node.persisted_control_state().revision, 0);
    // The old path does not implicitly opt into the new feature.
    node.mutate(command(1, 0)).await.unwrap();
    let member = crate::membership::issue_test_member(
        &identity,
        cluster,
        &identity,
        "tls://127.0.0.1:49993",
        crate::membership::initializer_capabilities(),
        42,
    );
    let mut publish = command(899, 0);
    publish.request = ControlCommandRequest::UpsertMember { member };
    node.mutate(publish).await.unwrap();
    node.activate_feature(FeatureActivationCommand {
        principal_id: "principal:test".into(),
        command_id: CommandId {
            value: uuid::Uuid::from_u128(900),
        },
        issued_at_unix_ms: 0,
        expected_control_revision: 2,
        read_schema_floor: 2,
        write_schema_floor: 2,
        feature: "bounded-control-command-v1".into(),
    })
    .await
    .unwrap();
    let accepted = node.mutate_bounded(command(2, 2)).await.unwrap();
    assert_eq!(accepted, node.mutate_bounded(command(2, 2)).await.unwrap());
    let revision = node.persisted_control_state().revision;
    let mut oversized = command(3, 3);
    oversized.request = ControlCommandRequest::CompleteWorkflow {
        original_command_id: CommandId {
            value: uuid::Uuid::from_u128(99),
        },
        response: vec![0; 1_048_576],
    };
    assert!(matches!(
        node.mutate_bounded(oversized).await,
        Err(ControlServiceError::Rejected { .. })
    ));
    assert_eq!(node.persisted_control_state().revision, revision);
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_publication_write_cannot_bypass_compatibility_gate() {
    let root = TempDir::new().unwrap();
    let network = MemoryNetwork::default();
    let node = start_node(root.path(), NodeId::from(4), &network, true).await;
    wait_for_leader(&[&node]).await;
    let before = node.persisted_control_state();
    let result = node.write(b"BMCAP001".to_vec()).await;
    assert!(
        matches!(result, Err(ConsensusWriteError::Membership(reason)) if reason == "capability publication requires membership-safe negotiated submission")
    );
    assert_eq!(node.persisted_control_state(), before);
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joiner_without_membership_rejects_authority() {
    use bmux_cluster_plugin_api::cluster_principal_bootstrap_types::{
        BootstrapProof, BootstrapStatement,
    };
    let root = TempDir::new().unwrap();
    let network = MemoryNetwork::default();
    let node = start_node(root.path(), NodeId::from(91), &network, false).await;
    assert!(!node.is_initialized());
    let result = node
        .bootstrap_principal(
            BootstrapProof {
                statement: BootstrapStatement {
                    schema_version: 1,
                    cluster_id: "cluster-test".into(),
                    membership_term: 0,
                    membership_leader_id: NodeId::from(91).to_string(),
                    membership_log_index: 0,
                    principal_id: uuid::Uuid::new_v4(),
                    principal_public_key: String::new(),
                    command_id: CommandId {
                        value: uuid::Uuid::new_v4(),
                    },
                },
                principal_signature: Vec::new(),
                voter_approvals: Vec::new(),
            },
            0,
        )
        .await;
    assert!(matches!(result, Err(ControlServiceError::NotLeader { .. })));
    assert!(matches!(
        node.mutate(command(1, 1)).await,
        Err(ControlServiceError::NotLeader { .. })
    ));
    assert!(node.persisted_control_state().principal_bootstrap.is_none());
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)] // One scenario spans formation through failover.
async fn three_voters_replicate_and_resume_after_leader_shutdown() {
    let network = MemoryNetwork::default();
    let roots = [
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    ];
    let ids = [NodeId::from(1), NodeId::from(2), NodeId::from(3)];
    let mut nodes = Vec::new();
    for index in 0..3 {
        nodes.push(start_node(roots[index].path(), ids[index], &network, index == 0).await);
    }
    assert_eq!(wait_for_leader(&[&nodes[0]]).await, ids[0]);
    nodes[0].mutate(command(1, 1)).await.unwrap();
    // Learners first, then joint-consensus promotion.
    nodes[0]
        .change_voters(ids.iter().map(|id| (*id, endpoint(*id))).collect())
        .await
        .unwrap();
    assert_eq!(nodes[0].committed_voter_ids(), ids.into_iter().collect());
    nodes[0].mutate(command(2, 2)).await.unwrap();
    for node in &nodes {
        wait_for_revision(node, 2).await;
        assert_eq!(node.committed_voter_ids(), ids.into_iter().collect());
    }
    // A follower names the leader and its endpoint.
    match nodes[1].mutate(command(3, 3)).await {
        Err(ControlServiceError::NotLeader {
            leader_node_id,
            leader_endpoint,
        }) => {
            assert_eq!(leader_node_id, Some(ids[0].to_string()));
            assert_eq!(leader_endpoint, Some(endpoint(ids[0])));
        }
        other => panic!("follower must redirect, got {other:?}"),
    }
    assert_eq!(nodes[1].read_stale_view().unwrap().revision, 2);

    // The leader stops; a survivor leads and the retry is deduplicated.
    nodes[0].shutdown().await.unwrap();
    network.unregister(ids[0]);
    let survivors = [&nodes[1], &nodes[2]];
    let leader = wait_for_leader(&survivors).await;
    let leader = survivors
        .iter()
        .find(|node| node.node_id() == leader)
        .unwrap();
    let response = leader.mutate(command(2, 2)).await.unwrap();
    assert_eq!(response.control_revision, 2, "retry returns the original");
    leader.mutate(command(4, 4)).await.unwrap();

    // The stopped voter restarts from its store and catches up.
    let restarted = start_node(roots[0].path(), ids[0], &network, true).await;
    wait_for_revision(&restarted, 3).await;
    for node in [&restarted, &nodes[1], &nodes[2]] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quorum_loss_rejects_writes_and_keeps_stale_reads() {
    let network = MemoryNetwork::default();
    let roots = [
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    ];
    let ids = [NodeId::from(11), NodeId::from(12), NodeId::from(13)];
    let mut nodes = Vec::new();
    for index in 0..3 {
        nodes.push(start_node(roots[index].path(), ids[index], &network, index == 0).await);
    }
    wait_for_leader(&[&nodes[0]]).await;
    nodes[0]
        .change_voters(ids.iter().map(|id| (*id, endpoint(*id))).collect())
        .await
        .unwrap();
    nodes[0].mutate(command(1, 1)).await.unwrap();
    network.isolate(ids[1]);
    network.isolate(ids[2]);
    let result = nodes[0].mutate(command(2, 2)).await;
    assert!(
        matches!(
            result,
            Err(ControlServiceError::QuorumUnavailable { .. }
                | ControlServiceError::NotLeader { .. })
        ),
        "a minority must not authorize a mutation: {result:?}"
    );
    assert_eq!(nodes[0].read_stale_view().unwrap().revision, 1);
    assert!(nodes[0].linearizable_control_state().await.is_err());
    network.heal(ids[1]);
    network.heal(ids[2]);
    let leader = wait_for_leader(&[&nodes[0], &nodes[1], &nodes[2]]).await;
    let leader = nodes.iter().find(|node| node.node_id() == leader).unwrap();
    leader.mutate(command(3, 3)).await.unwrap();
    for node in &nodes {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_store_is_rejected_without_modification() {
    let root = TempDir::new().unwrap();
    let legacy = root
        .path()
        .join("plugins/bmux.cluster/consensus/cluster-test");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("raft.redb"), b"legacy authority").unwrap();
    let network = MemoryNetwork::default();
    let result = ConsensusNode::start(
        root.path(),
        "cluster-test",
        NodeId::from(5),
        Provisioning::Founder {
            endpoint: "memory://5".into(),
        },
        &PreflightAdmission::default(),
        network.transport(NodeId::from(5)),
    )
    .await;
    let Err(ConsensusRuntimeError::Configuration(reason)) = result else {
        panic!("legacy authority must reject startup");
    };
    assert!(
        reason.contains("legacy OpenRaft consensus state"),
        "{reason}"
    );
    assert_eq!(
        std::fs::read(legacy.join("raft.redb")).unwrap(),
        b"legacy authority"
    );
    assert!(
        !owned_store_path(root.path(), "cluster-test").exists(),
        "no other store is initialized instead"
    );
}

#[tokio::test]
async fn unsupported_owned_store_is_rejected_without_modification() {
    let root = TempDir::new().unwrap();
    let path = owned_store_path(root.path(), "cluster-test");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, vec![7; 8192]).unwrap();
    let network = MemoryNetwork::default();
    let result = ConsensusNode::start(
        root.path(),
        "cluster-test",
        NodeId::from(6),
        Provisioning::Founder {
            endpoint: "memory://6".into(),
        },
        &PreflightAdmission::default(),
        network.transport(NodeId::from(6)),
    )
    .await;
    assert!(matches!(result, Err(ConsensusRuntimeError::Storage(_))));
    assert_eq!(std::fs::read(&path).unwrap(), vec![7; 8192]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_has_exactly_one_owner() {
    let root = TempDir::new().unwrap();
    let network = MemoryNetwork::default();
    let node = start_node(root.path(), NodeId::from(7), &network, true).await;
    let second = ConsensusNode::start(
        root.path(),
        "cluster-test",
        NodeId::from(7),
        Provisioning::Joiner,
        &PreflightAdmission::default(),
        network.transport(NodeId::from(7)),
    )
    .await;
    assert!(second.is_err(), "a second owner must not open the store");
    node.shutdown().await.unwrap();
}

#[test]
fn election_jitter_is_seeded_per_instance() {
    let (min, max) = (Duration::from_millis(1_000), Duration::from_millis(60_000));
    let mut first = seeded_jitter();
    let mut second = seeded_jitter();
    let a = (0..8).map(|_| first(min, max)).collect::<Vec<_>>();
    let b = (0..8).map(|_| second(min, max)).collect::<Vec<_>>();
    assert_ne!(a, b, "independent nodes must not share deadlines");
    assert!(a.iter().chain(&b).all(|draw| (min..=max).contains(draw)));
}
