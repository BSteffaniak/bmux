//! `OpenRaft` control-plane runtime wiring.
//!
//! This module owns node construction and lifecycle. Network implementations
//! remain injectable so production authenticated endpoint RPC and deterministic
//! in-memory tests exercise the same Raft/storage integration.

use crate::consensus_network::EndpointRaftNetworkFactory;
use crate::consensus_storage::{
    ConsensusLogStore, ConsensusStateMachine, ConsensusStorageError, ControlRaftConfig,
    ControlRequest,
};
use crate::control_state::ControlState;
use crate::membership::NodeId;
use bmux_cluster_plugin_api::cluster_types::{
    ControlReadConsistency, ControlResponse, ControlServiceError, ControlStateView,
};
use openraft::error::{CheckIsLeaderError, Fatal, ForwardToLeader, RaftError};
use openraft::network::RaftNetworkFactory;
use openraft::raft::ClientWriteResponse;
use openraft::{BasicNode, Config, SnapshotPolicy};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const SNAPSHOT_LOG_THRESHOLD: u64 = 1_024;

#[derive(Debug)]
pub enum ConsensusRuntimeError {
    Storage(ConsensusStorageError),
    Configuration(String),
    Fatal(Box<Fatal<NodeId>>),
}

impl std::fmt::Display for ConsensusRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "consensus storage failed: {error}"),
            Self::Configuration(error) => {
                write!(
                    formatter,
                    "consensus runtime configuration is invalid: {error}"
                )
            }
            Self::Fatal(error) => write!(formatter, "consensus runtime failed: {error}"),
        }
    }
}

impl std::error::Error for ConsensusRuntimeError {}

impl From<ConsensusStorageError> for ConsensusRuntimeError {
    fn from(error: ConsensusStorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<Fatal<NodeId>> for ConsensusRuntimeError {
    fn from(error: Fatal<NodeId>) -> Self {
        Self::Fatal(Box::new(error))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotLeader {
    pub leader_id: Option<NodeId>,
    pub leader_endpoint: Option<String>,
}

impl From<&ForwardToLeader<NodeId, BasicNode>> for NotLeader {
    fn from(forward: &ForwardToLeader<NodeId, BasicNode>) -> Self {
        Self {
            leader_id: forward.leader_id,
            leader_endpoint: forward.leader_node.as_ref().map(|node| node.addr.clone()),
        }
    }
}

#[derive(Debug)]
pub enum ConsensusWriteError {
    NotLeader(NotLeader),
    QuorumUnavailable(String),
    Fatal(Box<Fatal<NodeId>>),
    Membership(String),
}

impl std::fmt::Display for ConsensusWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLeader(forward) => match (forward.leader_id, &forward.leader_endpoint) {
                (Some(id), Some(endpoint)) => {
                    write!(formatter, "not leader; forward to {id} at {endpoint}")
                }
                (Some(id), None) => write!(formatter, "not leader; forward to {id}"),
                (None, _) => formatter.write_str("not leader; current leader is unknown"),
            },
            Self::QuorumUnavailable(reason) => {
                write!(formatter, "control-plane quorum is unavailable: {reason}")
            }
            Self::Fatal(error) => write!(formatter, "consensus runtime failed: {error}"),
            Self::Membership(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ConsensusWriteError {}

#[derive(Debug)]
pub enum ConsensusReadError {
    NotLeader(NotLeader),
    QuorumUnavailable(String),
    Fatal(Box<Fatal<NodeId>>),
    Storage(ConsensusStorageError),
}

impl std::fmt::Display for ConsensusReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLeader(forward) => match (forward.leader_id, &forward.leader_endpoint) {
                (Some(id), Some(endpoint)) => {
                    write!(formatter, "not leader; forward to {id} at {endpoint}")
                }
                (Some(id), None) => write!(formatter, "not leader; forward to {id}"),
                (None, _) => formatter.write_str("not leader; current leader is unknown"),
            },
            Self::QuorumUnavailable(error) => {
                write!(formatter, "linearizable read unavailable: {error}")
            }
            Self::Fatal(error) => write!(formatter, "consensus runtime failed: {error}"),
            Self::Storage(error) => write!(formatter, "consensus storage failed: {error}"),
        }
    }
}

impl std::error::Error for ConsensusReadError {}

/// One running `OpenRaft` node backed by cluster-owned durable storage.
#[derive(Clone)]
pub struct ConsensusNode {
    raft: openraft::Raft<ControlRaftConfig>,
    storage: ConsensusLogStore,
    node_id: NodeId,
}

impl ConsensusNode {
    /// Starts one Raft node over the durable storage for `cluster_id`.
    ///
    /// The supplied network factory determines transport only. All consensus,
    /// snapshot, compaction, and persistence behavior is identical across the
    /// production endpoint transport and deterministic test substitutions.
    /// # Errors
    ///
    /// Returns storage, configuration, or fatal Raft startup failures.
    pub async fn start<N>(
        state_dir: &Path,
        cluster_id: &str,
        node_id: NodeId,
        network: N,
    ) -> Result<Self, ConsensusRuntimeError>
    where
        N: RaftNetworkFactory<ControlRaftConfig>,
    {
        let storage = ConsensusLogStore::open(state_dir, cluster_id)?;
        let state_machine = storage.state_machine()?;
        let raft = openraft::Raft::new(
            node_id,
            consensus_config(cluster_id)?,
            network,
            storage.clone(),
            state_machine,
        )
        .await?;
        Ok(Self {
            raft,
            storage,
            node_id,
        })
    }

    /// Starts one production node using the authenticated connections-backed
    /// peer transport.
    ///
    /// # Errors
    ///
    /// Returns storage, configuration, or fatal Raft startup failures.
    pub async fn start_endpoint<C>(
        state_dir: &Path,
        cluster_id: &str,
        identity: crate::membership::NodeIdentity,
        caller: Arc<C>,
    ) -> Result<Self, ConsensusRuntimeError>
    where
        C: bmux_plugin::ServiceCaller + Send + Sync + 'static,
    {
        Self::start(
            state_dir,
            cluster_id,
            *identity.node_id(),
            EndpointRaftNetworkFactory::new(caller, identity),
        )
        .await
    }

    #[must_use]
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub const fn raft(&self) -> &openraft::Raft<ControlRaftConfig> {
        &self.raft
    }

    /// Initializes a pristine node as a single-voter cluster.
    ///
    /// # Errors
    ///
    /// Returns a Raft initialization error when durable state is non-pristine
    /// or the node cannot commit the initial membership.
    pub async fn initialize_single(
        &self,
        node_id: NodeId,
        endpoint: impl Into<String>,
    ) -> Result<(), RaftError<NodeId, openraft::error::InitializeError<NodeId, BasicNode>>> {
        self.raft
            .initialize(BTreeMap::from([(node_id, BasicNode::new(endpoint.into()))]))
            .await
    }

    /// Executes learner-first catch-up followed by `OpenRaft`'s joint and uniform
    /// voter transition. Every requested voter must have an authenticated
    /// endpoint address.
    ///
    /// The transition intentionally retains removed servers as learners so
    /// leave/revoke can first commit the safe voter-set change and then commit
    /// the replicated member-state update without making either step depend on
    /// an already-removed voter.
    ///
    /// # Errors
    ///
    /// Returns leader, quorum, membership, or fatal runtime failures.
    pub async fn change_voters(
        &self,
        voters: std::collections::BTreeMap<NodeId, BasicNode>,
    ) -> Result<ClientWriteResponse<ControlRaftConfig>, ConsensusWriteError> {
        let authenticated = voters
            .into_iter()
            .map(|(node_id, node)| {
                if node.addr.trim().is_empty() {
                    Err(ConsensusWriteError::Membership(format!(
                        "consensus voter {node_id} has no authenticated endpoint"
                    )))
                } else {
                    Ok((node_id, node))
                }
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
        if authenticated.is_empty() {
            return Err(ConsensusWriteError::Membership(
                "consensus voter set cannot be empty".to_string(),
            ));
        }
        match self.raft.ensure_linearizable().await {
            Ok(_) => {}
            Err(RaftError::Fatal(error)) => {
                return Err(ConsensusWriteError::Fatal(Box::new(error)));
            }
            Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(forward))) => {
                return Err(ConsensusWriteError::NotLeader((&forward).into()));
            }
            Err(RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(error))) => {
                return Err(ConsensusWriteError::QuorumUnavailable(error.to_string()));
            }
        }
        let voter_ids = authenticated
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        for (node_id, node) in authenticated {
            self.raft
                .add_learner(node_id, node, true)
                .await
                .map_err(client_write_error)?;
        }
        self.raft
            .change_membership(voter_ids, false)
            .await
            .map_err(client_write_error)
    }

    /// Executes a generated control mutation and returns its typed response.
    ///
    /// # Errors
    ///
    /// Returns structured leader, quorum, rejection, runtime, or internal errors.
    pub async fn mutate(
        &self,
        command: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> Result<ControlResponse, ControlServiceError> {
        let command_id = command.command_id.clone();
        let encoded = crate::control_codec::encode_control_command(&command);
        match self.write(encoded).await {
            Ok(response) => {
                let response = crate::control_codec::decode_control_response(&response.data.0)
                    .map_err(|error| ControlServiceError::Internal {
                        reason: format!("committed control response is invalid: {error}"),
                    })?;
                match &response.result {
                    bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Rejected {
                        error,
                    } => Err(ControlServiceError::Rejected {
                        error: error.clone(),
                    }),
                    bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Accepted {
                        ..
                    } => Ok(response),
                }
            }
            Err(ConsensusWriteError::NotLeader(not_leader)) => {
                Err(ControlServiceError::NotLeader {
                    leader_node_id: not_leader.leader_id.map(|id| id.to_string()),
                    leader_endpoint: not_leader.leader_endpoint,
                })
            }
            Err(ConsensusWriteError::QuorumUnavailable(reason)) => {
                Err(ControlServiceError::QuorumUnavailable { reason })
            }
            Err(ConsensusWriteError::Fatal(error)) => Err(ControlServiceError::Internal {
                reason: format!(
                    "consensus runtime failed for command {}: {error}",
                    command_id.value
                ),
            }),
            Err(ConsensusWriteError::Membership(reason)) => {
                Err(ControlServiceError::RuntimeUnavailable { reason })
            }
        }
    }

    /// Returns a quorum-confirmed generated state view.
    ///
    /// # Errors
    ///
    /// Returns structured leader, quorum, runtime, or storage errors.
    pub async fn read_linearizable_view(&self) -> Result<ControlStateView, ControlServiceError> {
        self.linearizable_control_state()
            .await
            .map(|state| state.to_view(ControlReadConsistency::Linearizable))
            .map_err(control_read_error)
    }

    /// Returns an explicitly stale generated state view.
    ///
    /// # Errors
    ///
    /// Returns an internal error when durable state cannot be decoded.
    pub fn read_stale_view(&self) -> Result<ControlStateView, ControlServiceError> {
        self.persisted_control_state()
            .map(|state| state.to_view(ControlReadConsistency::Stale))
            .map_err(|error| ControlServiceError::Internal {
                reason: error.to_string(),
            })
    }

    /// Submits one canonical control command to the current leader.
    ///
    /// # Errors
    ///
    /// Returns actionable leader guidance, quorum unavailability, membership
    /// errors, or fatal Raft failures. A command is never replayed internally
    /// after dispatch.
    pub async fn write(
        &self,
        command: Vec<u8>,
    ) -> Result<ClientWriteResponse<ControlRaftConfig>, ConsensusWriteError> {
        match self.raft.ensure_linearizable().await {
            Ok(_) => {}
            Err(RaftError::Fatal(error)) => {
                return Err(ConsensusWriteError::Fatal(Box::new(error)));
            }
            Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(forward))) => {
                return Err(ConsensusWriteError::NotLeader((&forward).into()));
            }
            Err(RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(error))) => {
                return Err(ConsensusWriteError::QuorumUnavailable(error.to_string()));
            }
        }
        match self.raft.client_write(ControlRequest(command)).await {
            Ok(response) => Ok(response),
            Err(error) => Err(client_write_error(error)),
        }
    }

    /// Confirms leadership and quorum before returning the durable state.
    ///
    /// # Errors
    ///
    /// Returns actionable leader guidance, quorum unavailability, fatal Raft
    /// failures, or durable state read failures.
    pub async fn linearizable_control_state(&self) -> Result<ControlState, ConsensusReadError> {
        match self.raft.ensure_linearizable().await {
            Ok(_) => self
                .persisted_control_state()
                .map_err(ConsensusReadError::Storage),
            Err(error) => match error {
                RaftError::Fatal(error) => Err(ConsensusReadError::Fatal(Box::new(error))),
                RaftError::APIError(CheckIsLeaderError::ForwardToLeader(forward)) => {
                    Err(ConsensusReadError::NotLeader((&forward).into()))
                }
                RaftError::APIError(CheckIsLeaderError::QuorumNotEnough(error)) => {
                    Err(ConsensusReadError::QuorumUnavailable(error.to_string()))
                }
            },
        }
    }

    /// Returns a durable state-machine view without a quorum confirmation.
    ///
    /// Callers must explicitly label this result stale when presenting it.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable state machine cannot be decoded.
    pub fn persisted_control_state(&self) -> Result<ControlState, ConsensusStorageError> {
        self.storage
            .state_machine()
            .map(|state_machine: ConsensusStateMachine| state_machine.control_state().clone())
    }

    /// Gracefully stops the node and waits for its Raft task to exit.
    ///
    /// # Errors
    ///
    /// Returns the host runtime join failure if the Raft task cannot be joined.
    pub async fn shutdown(
        &self,
    ) -> Result<(), <openraft::TokioRuntime as openraft::AsyncRuntime>::JoinError> {
        self.raft.shutdown().await
    }
}

fn client_write_error(
    error: RaftError<NodeId, openraft::error::ClientWriteError<NodeId, BasicNode>>,
) -> ConsensusWriteError {
    error.forward_to_leader::<BasicNode>().cloned().map_or_else(
        || match error {
            RaftError::Fatal(error) => ConsensusWriteError::Fatal(Box::new(error)),
            RaftError::APIError(error) => ConsensusWriteError::Membership(error.to_string()),
        },
        |forward| ConsensusWriteError::NotLeader((&forward).into()),
    )
}

fn control_read_error(error: ConsensusReadError) -> ControlServiceError {
    match error {
        ConsensusReadError::NotLeader(not_leader) => ControlServiceError::NotLeader {
            leader_node_id: not_leader.leader_id.map(|id| id.to_string()),
            leader_endpoint: not_leader.leader_endpoint,
        },
        ConsensusReadError::QuorumUnavailable(reason) => {
            ControlServiceError::QuorumUnavailable { reason }
        }
        ConsensusReadError::Fatal(error) => ControlServiceError::Internal {
            reason: error.to_string(),
        },
        ConsensusReadError::Storage(error) => ControlServiceError::Internal {
            reason: error.to_string(),
        },
    }
}

fn consensus_config(cluster_id: &str) -> Result<Arc<Config>, ConsensusRuntimeError> {
    let config = Config {
        cluster_name: cluster_id.to_owned(),
        heartbeat_interval: 50,
        election_timeout_min: 150,
        election_timeout_max: 300,
        snapshot_policy: SnapshotPolicy::LogsSinceLast(SNAPSHOT_LOG_THRESHOLD),
        max_in_snapshot_log_to_keep: 0,
        purge_batch_size: 64,
        ..Config::default()
    }
    .validate()
    .map_err(|error| ConsensusRuntimeError::Configuration(error.to_string()))?;
    Ok(Arc::new(config))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::control_codec::{decode_control_response, encode_control_command};
    use bmux_cluster_plugin_api::cluster_types::{
        CommandId, ControlCommand, ControlCommandRequest, WorkspaceId,
    };
    use openraft::error::{RPCError, RemoteError, Unreachable};
    use openraft::network::{RPCOption, RaftNetwork};
    use openraft::raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::io;
    use std::sync::RwLock;
    use std::time::Duration;
    use tempfile::TempDir;

    type RegisteredNode = (BasicNode, openraft::Raft<ControlRaftConfig>);

    #[derive(Clone, Default)]
    pub struct InMemoryNetworkFactory {
        nodes: Arc<RwLock<BTreeMap<NodeId, RegisteredNode>>>,
    }

    impl InMemoryNetworkFactory {
        pub fn register(
            &self,
            id: NodeId,
            node: BasicNode,
            raft: openraft::Raft<ControlRaftConfig>,
        ) {
            self.nodes
                .write()
                .expect("network registry lock poisoned")
                .insert(id, (node, raft));
        }
    }

    pub struct InMemoryNetwork {
        target: NodeId,
        expected: BasicNode,
        nodes: Arc<RwLock<BTreeMap<NodeId, RegisteredNode>>>,
    }

    #[allow(clippy::result_large_err)] // OpenRaft's transport error preserves typed remote context.
    impl InMemoryNetwork {
        fn remote<E: std::error::Error>(
            &self,
        ) -> Result<openraft::Raft<ControlRaftConfig>, RPCError<NodeId, BasicNode, E>> {
            let Some((actual, raft)) = self
                .nodes
                .read()
                .expect("network registry lock poisoned")
                .get(&self.target)
                .cloned()
            else {
                return Err(Unreachable::new(&io::Error::new(
                    io::ErrorKind::NotConnected,
                    "target node is not registered",
                ))
                .into());
            };
            if actual != self.expected {
                return Err(Unreachable::new(&io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "target node identity does not match its registered endpoint",
                ))
                .into());
            }
            Ok(raft)
        }
    }

    impl RaftNetworkFactory<ControlRaftConfig> for InMemoryNetworkFactory {
        type Network = InMemoryNetwork;

        async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
            InMemoryNetwork {
                target,
                expected: node.clone(),
                nodes: self.nodes.clone(),
            }
        }
    }

    impl RaftNetwork<ControlRaftConfig> for InMemoryNetwork {
        async fn append_entries(
            &mut self,
            rpc: AppendEntriesRequest<ControlRaftConfig>,
            _option: RPCOption,
        ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>>
        {
            let remote = self.remote()?;
            remote.append_entries(rpc).await.map_err(|error| {
                RemoteError::new_with_node(self.target, self.expected.clone(), error).into()
            })
        }

        async fn install_snapshot(
            &mut self,
            rpc: InstallSnapshotRequest<ControlRaftConfig>,
            _option: RPCOption,
        ) -> Result<
            InstallSnapshotResponse<NodeId>,
            RPCError<NodeId, BasicNode, RaftError<NodeId, openraft::error::InstallSnapshotError>>,
        > {
            let remote = self.remote()?;
            remote.install_snapshot(rpc).await.map_err(|error| {
                RemoteError::new_with_node(self.target, self.expected.clone(), error).into()
            })
        }

        async fn vote(
            &mut self,
            rpc: VoteRequest<NodeId>,
            _option: RPCOption,
        ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
            let remote = self.remote()?;
            remote.vote(rpc).await.map_err(|error| {
                RemoteError::new_with_node(self.target, self.expected.clone(), error).into()
            })
        }
    }

    pub async fn wait_for_leader(nodes: &[&ConsensusNode]) -> NodeId {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                for node in nodes {
                    if let Some(leader) = node.raft().current_leader().await {
                        return leader;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cluster should elect a leader")
    }

    async fn wait_for_leader_excluding(nodes: &[&ConsensusNode], excluded: NodeId) -> NodeId {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                for node in nodes {
                    if let Some(leader) = node.raft().current_leader().await
                        && leader != excluded
                    {
                        return leader;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("surviving quorum should elect a replacement leader")
    }

    fn command(id: u128, workspace: u128) -> ControlCommand {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)] // One scenario intentionally spans formation through failover.
    async fn three_voters_replicate_dedup_and_resume_after_leader_shutdown() {
        let network = InMemoryNetworkFactory::default();
        let roots = [
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        ];
        let ids = [NodeId::from(1), NodeId::from(2), NodeId::from(3)];
        let endpoints = ["memory://one", "memory://two", "memory://three"];
        let mut nodes = Vec::new();
        for index in 0..3 {
            let node = ConsensusNode::start(
                roots[index].path(),
                "cluster-runtime-test",
                ids[index],
                network.clone(),
            )
            .await
            .unwrap();
            network.register(
                ids[index],
                BasicNode::new(endpoints[index]),
                node.raft().clone(),
            );
            nodes.push(node);
        }

        nodes[0]
            .initialize_single(ids[0], endpoints[0])
            .await
            .unwrap();
        assert_eq!(wait_for_leader(&[&nodes[0]]).await, ids[0]);
        nodes[0]
            .raft()
            .add_learner(ids[1], BasicNode::new(endpoints[1]), true)
            .await
            .unwrap();
        nodes[0]
            .raft()
            .add_learner(ids[2], BasicNode::new(endpoints[2]), true)
            .await
            .unwrap();
        nodes[0]
            .raft()
            .change_membership(BTreeSet::from(ids), false)
            .await
            .unwrap();

        let first = command(1, 10);
        let encoded = encode_control_command(&first);
        let response = nodes[0].write(encoded.clone()).await.unwrap();
        let decoded = decode_control_response(&response.data.0).unwrap();
        assert_eq!(decoded.command_id, first.command_id);
        let replay = nodes[0].write(encoded).await.unwrap();
        assert_eq!(replay.data, response.data);

        let follower = &nodes[1];
        assert!(matches!(
            follower.write(encode_control_command(&command(99, 99))).await,
            Err(ConsensusWriteError::NotLeader(NotLeader {
                leader_id: Some(id),
                leader_endpoint: Some(endpoint),
            })) if id == ids[0] && endpoint == endpoints[0]
        ));
        let linear = nodes[0].read_linearizable_view().await.unwrap();
        assert_eq!(linear.consistency, ControlReadConsistency::Linearizable);
        assert!(
            linear
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id.value == first_workspace())
        );
        let stale = follower.read_stale_view().unwrap();
        assert_eq!(stale.consistency, ControlReadConsistency::Stale);
        assert!(matches!(
            follower.linearizable_control_state().await,
            Err(ConsensusReadError::NotLeader(NotLeader {
                leader_id: Some(id),
                leader_endpoint: Some(endpoint),
            })) if id == ids[0] && endpoint == endpoints[0]
        ));

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if nodes.iter().all(|node| {
                    node.persisted_control_state()
                        .is_ok_and(|state| state.workspaces.contains_key(&first_workspace()))
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("all voters should apply the first command");

        nodes[0].shutdown().await.unwrap();
        let replacement = wait_for_leader_excluding(&[&nodes[1], &nodes[2]], ids[0]).await;
        assert_ne!(replacement, ids[0]);
        let leader = if replacement == ids[1] {
            &nodes[1]
        } else {
            &nodes[2]
        };
        let second = command(2, 20);
        leader.write(encode_control_command(&second)).await.unwrap();
        let linear = leader.read_linearizable_view().await.unwrap();
        assert!(
            linear
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id.value == second_workspace())
        );

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if nodes[1..].iter().all(|node| {
                    node.persisted_control_state()
                        .is_ok_and(|state| state.workspaces.contains_key(&second_workspace()))
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("surviving quorum should apply a command after leader loss");

        nodes[1].shutdown().await.unwrap();
        nodes[2].shutdown().await.unwrap();
    }

    const fn first_workspace() -> uuid::Uuid {
        uuid::Uuid::from_u128(10)
    }

    const fn second_workspace() -> uuid::Uuid {
        uuid::Uuid::from_u128(20)
    }

    struct FailingCaller;

    impl bmux_plugin::ServiceCaller for FailingCaller {
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: bmux_plugin_sdk::ServiceKind,
            _interface_id: &str,
            _operation: &str,
            _payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "call_service_raw",
            })
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "execute_kernel_request",
            })
        }
    }

    #[tokio::test]
    async fn change_voters_catches_up_learners_and_removes_old_voters() {
        let network = InMemoryNetworkFactory::default();
        let roots = [
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        ];
        let ids = [NodeId::from(81), NodeId::from(82), NodeId::from(83)];
        let endpoints = ["memory://m1", "memory://m2", "memory://m3"];
        let mut nodes = Vec::new();
        for index in 0..3 {
            let node = ConsensusNode::start(
                roots[index].path(),
                "cluster-voter-change-test",
                ids[index],
                network.clone(),
            )
            .await
            .unwrap();
            network.register(
                ids[index],
                BasicNode::new(endpoints[index]),
                node.raft().clone(),
            );
            nodes.push(node);
        }
        nodes[0]
            .initialize_single(ids[0], endpoints[0])
            .await
            .unwrap();
        wait_for_leader(&[&nodes[0]]).await;
        nodes[0]
            .change_voters(BTreeMap::from([
                (ids[0], BasicNode::new(endpoints[0])),
                (ids[1], BasicNode::new(endpoints[1])),
            ]))
            .await
            .unwrap();
        assert_eq!(wait_for_leader(&[&nodes[0], &nodes[1]]).await, ids[0]);
        nodes[0]
            .change_voters(BTreeMap::from([
                (ids[1], BasicNode::new(endpoints[1])),
                (ids[2], BasicNode::new(endpoints[2])),
            ]))
            .await
            .unwrap();
        nodes[0].shutdown().await.unwrap();
        let replacement = wait_for_leader(&[&nodes[1], &nodes[2]]).await;
        assert!(replacement == ids[1] || replacement == ids[2]);
        let leader = if replacement == ids[1] {
            &nodes[1]
        } else {
            &nodes[2]
        };
        leader
            .mutate(command(82, 820))
            .await
            .expect("new voter set should commit after old voter removal");
        nodes[1].shutdown().await.unwrap();
        nodes[2].shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn quorum_loss_rejects_writes_and_preserves_explicit_stale_reads() {
        let network = InMemoryNetworkFactory::default();
        let roots = [
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        ];
        let ids = [NodeId::from(41), NodeId::from(42), NodeId::from(43)];
        let endpoints = ["memory://q1", "memory://q2", "memory://q3"];
        let mut nodes = Vec::new();
        for index in 0..3 {
            let node = ConsensusNode::start(
                roots[index].path(),
                "cluster-quorum-loss-test",
                ids[index],
                network.clone(),
            )
            .await
            .unwrap();
            network.register(
                ids[index],
                BasicNode::new(endpoints[index]),
                node.raft().clone(),
            );
            nodes.push(node);
        }

        nodes[0]
            .initialize_single(ids[0], endpoints[0])
            .await
            .unwrap();
        wait_for_leader(&[&nodes[0]]).await;
        for index in 1..3 {
            nodes[0]
                .raft()
                .add_learner(ids[index], BasicNode::new(endpoints[index]), true)
                .await
                .unwrap();
        }
        nodes[0]
            .raft()
            .change_membership(BTreeSet::from(ids), false)
            .await
            .unwrap();
        nodes[0]
            .mutate(command(70, 700))
            .await
            .expect("quorum should commit baseline command");

        nodes[1].shutdown().await.unwrap();
        nodes[2].shutdown().await.unwrap();

        let error = tokio::time::timeout(Duration::from_secs(5), nodes[0].mutate(command(71, 701)))
            .await
            .expect("quorum failure should be bounded")
            .expect_err("minority leader must reject writes");
        assert!(matches!(
            error,
            ControlServiceError::QuorumUnavailable { .. }
        ));

        let linear =
            tokio::time::timeout(Duration::from_secs(5), nodes[0].read_linearizable_view())
                .await
                .expect("linearizable read failure should be bounded")
                .expect_err("minority must reject linearizable reads");
        assert!(matches!(
            linear,
            ControlServiceError::QuorumUnavailable { .. }
        ));

        let stale = nodes[0].read_stale_view().unwrap();
        assert_eq!(stale.consistency, ControlReadConsistency::Stale);
        assert!(
            stale
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id.value == uuid::Uuid::from_u128(700))
        );
        assert!(
            !stale
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id.value == uuid::Uuid::from_u128(701))
        );

        nodes[0].shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn network_rejects_wrong_endpoint_for_expected_node() {
        let network = InMemoryNetworkFactory::default();
        let root = TempDir::new().unwrap();
        let id = NodeId::from(9);
        let node = ConsensusNode::start(
            root.path(),
            "cluster-runtime-identity-test",
            id,
            network.clone(),
        )
        .await
        .unwrap();
        network.register(id, BasicNode::new("memory://actual"), node.raft().clone());
        let mut factory = network;
        let client = factory
            .new_client(id, &BasicNode::new("memory://forged"))
            .await;
        assert!(client.remote::<RaftError<NodeId>>().is_err());
        let service = crate::consensus_network::ControlServiceHandle::new(
            Arc::new(FailingCaller),
            id,
            crate::consensus_network::global_consensus_nodes(),
        );
        assert!(matches!(
            service.active(),
            Err(ControlServiceError::RuntimeUnavailable { .. })
        ));
        node.shutdown().await.unwrap();
    }
}
