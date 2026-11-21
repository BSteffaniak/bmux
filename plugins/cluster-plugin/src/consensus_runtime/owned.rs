//! Qualification-only owned lifecycle over the real owned authority.
//!
//! Fixed 3/5-voter qualification supports explicit campaigns and an automatic
//! lifecycle driven only by executor-authenticated election activity.
//! No production registry, endpoint provider, or capability is installed here.
//! The executor is exclusively transferred to this task. Durable application
//! results are correlated by full log identity, never response-vector position.

use crate::consensus_model::{LogIdentity, Membership, ReadContext, RequestVote};
use crate::control_state::ControlState;
use crate::membership::NodeId;
use crate::owned_vote_store::{AuthorizedRead, OwnedObservation, ProposalError, VoteExecutor};
use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
use bmux_cluster_plugin_api::cluster_types::{ControlCommand, ControlResponse};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

const ADMISSION_CAPACITY: usize = 8;
const COMMAND_BYTES: usize = 1_048_576;
const APPLICATION_TURNS: usize = 256;
const COMPACTION_INTERVAL: u64 = 128;
// Large entries can exhaust byte capacity long before the entry-count cadence.
const LARGE_COMMAND_BYTES: usize = 256 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
// Four remote voters in the currently supported five-voter configuration.
// This stays below the executor's existing sixteen outbound admissions.
const PEER_TASK_CAPACITY: usize = 4;
// Measurement candidates, not ADR-0008 performance qualification.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(75);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(25);
const ELECTION_MIN_MILLIS: u64 = 450;
const ELECTION_JITTER_MILLIS: u64 = 450;

struct AutomaticElection {
    deadline: tokio::time::Instant,
    activity_sequence: Option<u64>,
    history: std::collections::VecDeque<String>,
    jitter_state: [u8; 32],
}

impl AutomaticElection {
    fn timeout(&mut self) -> Duration {
        use sha2::Digest as _;
        // A per-instance entropy seed with a deterministic hash stream makes
        // timing reproducible without sharing startup/retry jitter across nodes.
        self.jitter_state = sha2::Sha256::digest(self.jitter_state).into();
        let random = u64::from_le_bytes(self.jitter_state[..8].try_into().expect("eight bytes"));
        Duration::from_millis(ELECTION_MIN_MILLIS + random % ELECTION_JITTER_MILLIS)
    }

    fn seeded(now: tokio::time::Instant, seed: [u8; 32]) -> Self {
        let mut election = Self {
            deadline: now,
            activity_sequence: None,
            history: std::collections::VecDeque::new(),
            jitter_state: seed,
        };
        election.reset(now);
        election
    }

    fn reset(&mut self, base: tokio::time::Instant) {
        self.deadline = base + self.timeout();
    }

    fn record(&mut self, event: String) {
        if self.history.len() == 16 {
            self.history.pop_front();
        }
        self.history.push_back(event);
    }

    fn observe_activity(&mut self, activity: crate::owned_vote_store::ElectionActivity) -> bool {
        if self.activity_sequence == Some(activity.sequence) {
            return false;
        }
        if activity.kind == crate::owned_vote_store::ElectionActivityKind::GrantedVote {
            self.record(format!(
                "granted term={:?} peer={} seq={}",
                activity.term, activity.peer, activity.sequence
            ));
        }
        self.activity_sequence = Some(activity.sequence);
        self.reset(tokio::time::Instant::from_std(activity.observed_at));
        true
    }

    fn new() -> Self {
        let mut seed = [0; 32];
        seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self::seeded(tokio::time::Instant::now(), seed)
    }
}

type Authorize = Box<dyn FnOnce(&ControlState, &ControlCommand) -> Result<(), String> + Send>;

type PeerWork = Pin<Box<dyn Future<Output = Result<(), RpcError>> + Send>>;

/// Only the authenticated generated transport is substituted, never consensus
/// decisions. All quorum/progress observations remain in the shared executor.
trait OwnedTransport: Send + Sync {
    fn vote(&self, executor: Arc<VoteExecutor>, request: RequestVote, peer: NodeId) -> PeerWork;
    fn confirm(&self, executor: Arc<VoteExecutor>, context: ReadContext, peer: NodeId) -> PeerWork;
    fn replicate(&self, executor: Arc<VoteExecutor>, peer: NodeId) -> PeerWork;
}

struct EndpointTransport<C> {
    caller: Arc<C>,
    local: NodeId,
}

impl<C> OwnedTransport for EndpointTransport<C>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
{
    fn vote(&self, executor: Arc<VoteExecutor>, campaign: RequestVote, peer: NodeId) -> PeerWork {
        let caller = self.caller.clone();
        let local = self.local;
        Box::pin(async move {
            use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
                LogIdentity as WireIdentity, TransferProof,
            };
            let identity = crate::load_or_create_node_identity(caller.as_ref())
                .map_err(|_| RpcError::Unauthorized)?;
            if *identity.node_id() != local {
                return Err(RpcError::Unauthorized);
            }
            let state = executor
                .read_control_state()
                .await
                .map_err(|error| match error {
                    crate::owned_vote_store::AppendExecutionError::Executor(
                        crate::consensus_model::VoteGateError::Busy,
                    ) => RpcError::CapacityUnavailable,
                    crate::owned_vote_store::AppendExecutionError::Executor(
                        crate::consensus_model::VoteGateError::Stopped,
                    ) => RpcError::Stopped,
                    _ => RpcError::StorageFailure,
                })?;
            let member = state
                .members
                .get(&local.to_string())
                .ok_or(RpcError::Unauthorized)?;
            let endpoint = state
                .members
                .get(&peer.to_string())
                .and_then(|member| member.endpoint.clone())
                .ok_or(RpcError::Unauthorized)?;
            let proof = tokio::time::timeout(
                Duration::from_secs(5),
                crate::endpoint::peer_authentication_proof(
                    caller.as_ref(),
                    &endpoint,
                    &local.to_string(),
                    &peer.to_string(),
                ),
            )
            .await
            .map_err(|_| RpcError::ExecutionFailed)?
            .map_err(|_| RpcError::Unauthorized)?;
            let mut request = bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest {
                cluster_id: state.cluster_id.clone(),
                sender: local.as_bytes().to_vec(),
                recipient: peer.as_bytes().to_vec(),
                request_id: uuid::Uuid::new_v4(),
                term: campaign.term.0,
                last_log: campaign.last_log.map(|position| WireIdentity {
                    term: position.term.0,
                    leader: position.leader.as_bytes().to_vec(),
                    index: position.index.0,
                }),
                proof: TransferProof {
                    credential_serial: member.credential_serial.clone(),
                    signature: Vec::new(),
                    peer: proof,
                },
            };
            request.proof.signature = identity.sign(
                &crate::consensus_network::owned_vote_signing_payload(&request)
                    .map_err(|_| RpcError::InvalidRequest)?,
            );
            executor
                .request_campaign_vote(caller.as_ref(), local, request)
                .await
                .map(|_| ())
        })
    }

    fn confirm(&self, executor: Arc<VoteExecutor>, context: ReadContext, peer: NodeId) -> PeerWork {
        let caller = self.caller.clone();
        Box::pin(async move { executor.confirm_read(caller.as_ref(), context, peer).await })
    }

    fn replicate(&self, executor: Arc<VoteExecutor>, peer: NodeId) -> PeerWork {
        let caller = self.caller.clone();
        Box::pin(async move {
            // One bounded probe per scheduling turn: an eight-probe catch-up
            // loop must not monopolize preparation/settlement while other
            // voters need their heartbeat. Only explicit capacity rejection is
            // retried here; transport/authority outcomes leave this turn.
            for attempt in 0..4 {
                match executor.replicate_next(caller.as_ref(), peer).await {
                    Err(RpcError::CapacityUnavailable) if attempt < 3 => {
                        let millis = u64::from(uuid::Uuid::new_v4().as_bytes()[0] % 4) + 1;
                        tokio::time::sleep(Duration::from_millis(millis)).await;
                    }
                    result => return result,
                }
            }
            unreachable!("bounded probe admission always returns")
        })
    }
}

struct PendingProposal {
    identity: LogIdentity,
    principal: String,
    command: Box<ControlCommand>,
    response: oneshot::Sender<Result<ControlResponse, RuntimeError>>,
    deadline: tokio::time::Instant,
}

struct RecoveringExecutor(Option<Arc<VoteExecutor>>);

impl Drop for RecoveringExecutor {
    fn drop(&mut self) {
        if let Some(executor) = self.0.take() {
            // Cancellation while validating/registering startup cannot strand
            // the exclusive store. This guard is disarmed after task handoff.
            tokio::spawn(async move {
                if let Err(error) = executor.shutdown().await {
                    tracing::error!(?error, "cancelled owned startup cleanup failed");
                }
            });
        }
    }
}

struct PendingRead {
    context: ReadContext,
    response: oneshot::Sender<Result<AuthorizedRead, RuntimeError>>,
    deadline: tokio::time::Instant,
    attempted: BTreeSet<NodeId>,
}

struct FixedPeers {
    automatic: Option<AutomaticElection>,
    next_heartbeat: tokio::time::Instant,
    read_turn: bool,
    cursor: usize,
    recent: std::collections::VecDeque<String>,
    ticks: u64,
    busy_ticks: u64,
    read: Option<PendingRead>,
    membership: Membership,
    transport: Arc<dyn OwnedTransport>,
    tasks: JoinSet<(NodeId, Result<(), RpcError>)>,
    in_flight: BTreeSet<NodeId>,
    campaign: Option<RequestVote>,
    pending: BTreeMap<(u64, NodeId, u64), PendingProposal>,
}

impl FixedPeers {
    fn preparing(&self) -> bool {
        !self.tasks.is_empty()
            && (self.automatic.is_none() || tokio::time::Instant::now() < self.next_heartbeat)
    }

    fn expire_waiters(&mut self, now: tokio::time::Instant) {
        let expired = self
            .pending
            .iter()
            .filter_map(|(key, pending)| {
                (pending.deadline <= now || pending.response.is_closed()).then_some(*key)
            })
            .collect::<Vec<_>>();
        for key in expired {
            if let Some(pending) = self.pending.remove(&key) {
                // Cancellation removes waiting only, never durable intent.
                let _ = pending.response.send(Err(RuntimeError::Indeterminate));
            }
        }
        if self
            .read
            .as_ref()
            .is_some_and(|read| read.deadline <= now || read.response.is_closed())
            && let Some(read) = self.read.take()
        {
            let _ = read.response.send(Err(RuntimeError::QuorumUnavailable));
        }
    }

    fn completed(&mut self, peer: NodeId, result: Result<(), RpcError>) {
        if self.recent.len() == 16 {
            self.recent.pop_front();
        }
        self.recent.push_back(format!("{peer}: {result:?}"));
        self.in_flight.remove(&peer);
        if result.is_err()
            && let Some(read) = self.read.as_mut()
        {
            read.attempted.remove(&peer);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeError {
    Capacity,
    QuorumUnavailable,
    Stopped,
    Rejected(String),
    UnsupportedMembership,
    Authority(String),
    /// Waiting ended after admission; the command may have become durable.
    /// Retry only with the same principal, identity, and canonical request.
    Indeterminate,
}

enum Request {
    Submit {
        principal: String,
        command: Box<ControlCommand>,
        authorize: Authorize,
        response: oneshot::Sender<Result<ControlResponse, RuntimeError>>,
    },
    Diagnostics(oneshot::Sender<String>),
    ReadLinearizable(oneshot::Sender<Result<AuthorizedRead, RuntimeError>>),
    Campaign(oneshot::Sender<Result<(), RuntimeError>>),
    Observe(oneshot::Sender<Result<OwnedObservation, RuntimeError>>),
    ReadStale(oneshot::Sender<Result<ControlState, RuntimeError>>),
}

struct Task {
    handle: Option<JoinHandle<Result<(), RuntimeError>>>,
    result: Option<Result<(), RuntimeError>>,
}

struct FixedRecovery {
    path: PathBuf,
    cluster: String,
    local: NodeId,
    incarnation: u128,
    automatic: bool,
}

struct OwnedRuntime {
    requests: mpsc::Sender<Request>,
    stop: watch::Sender<bool>,
    task: Mutex<Task>,
}

impl OwnedRuntime {
    /// Recover an explicitly selected owned-format store. This never creates,
    /// imports, opens legacy storage, or joins the production node registry.
    async fn recover(
        path: PathBuf,
        cluster: String,
        local: NodeId,
        incarnation: u128,
    ) -> Result<Self, RuntimeError> {
        let executor = Arc::new(
            VoteExecutor::recover(path, cluster, incarnation)
                .await
                .map_err(|error| RuntimeError::Authority(error.to_string()))?,
        );
        let (requests, receive) = mpsc::channel(ADMISSION_CAPACITY);
        let (stop, stopping) = watch::channel(false);
        let (ready, readiness) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let result = run(&executor, local, receive, stopping, ready, None).await;
            // Always release durable ownership, including failed readiness and
            // cancellation of the caller awaiting startup. No task is aborted.
            let cleanup = executor.shutdown().await.map_err(authority);
            cleanup.and(result)
        });
        let runtime = Self {
            requests,
            stop,
            task: Mutex::new(Task {
                handle: Some(handle),
                result: None,
            }),
        };
        match readiness.await {
            Ok(Ok(())) => Ok(runtime),
            Ok(Err(error)) => {
                runtime.shutdown().await?;
                Err(error)
            }
            Err(_) => {
                runtime.shutdown().await?;
                Err(RuntimeError::Stopped)
            }
        }
    }

    /// Fixed membership qualification entry point. The caller installs/removes
    /// `OwnedAppendService` against this exact executor before/after task life;
    /// the returned guard owns registration and is dropped before store close.
    async fn recover_fixed<C, G, F>(
        path: PathBuf,
        cluster: String,
        local: NodeId,
        incarnation: u128,
        caller: Arc<C>,
        register: F,
    ) -> Result<Self, RuntimeError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
        G: Send + 'static,
        F: FnOnce(Arc<VoteExecutor>) -> Result<G, RuntimeError>,
    {
        Self::recover_peers(
            FixedRecovery {
                path,
                cluster,
                local,
                incarnation,
                automatic: false,
            },
            caller,
            register,
        )
        .await
    }

    /// Automatic elections and heartbeats over the same authenticated authority
    /// as fixed qualification. Dynamic/joint membership is deliberately rejected.
    async fn recover_automatic<C, G, F>(
        path: PathBuf,
        cluster: String,
        local: NodeId,
        incarnation: u128,
        caller: Arc<C>,
        register: F,
    ) -> Result<Self, RuntimeError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
        G: Send + 'static,
        F: FnOnce(Arc<VoteExecutor>) -> Result<G, RuntimeError>,
    {
        Self::recover_peers(
            FixedRecovery {
                path,
                cluster,
                local,
                incarnation,
                automatic: true,
            },
            caller,
            register,
        )
        .await
    }

    async fn recover_peers<C, G, F>(
        recovery: FixedRecovery,
        caller: Arc<C>,
        register: F,
    ) -> Result<Self, RuntimeError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
        G: Send + 'static,
        F: FnOnce(Arc<VoteExecutor>) -> Result<G, RuntimeError>,
    {
        let FixedRecovery {
            path,
            cluster,
            local,
            incarnation,
            automatic,
        } = recovery;
        let executor = Arc::new(
            VoteExecutor::recover(path, cluster, incarnation)
                .await
                .map_err(recovery_error)?,
        );
        let mut recovering = RecoveringExecutor(Some(executor.clone()));
        let observation = match executor.observe(local).await {
            Ok(observation) => observation,
            Err(error) => {
                executor.shutdown().await.map_err(authority)?;
                return Err(authority(error));
            }
        };
        let membership = observation
            .effective_membership
            .as_ref()
            .map(|m| m.membership.clone());
        let valid = membership.as_ref().is_some_and(|m| {
            m.configurations().len() == 1
                && matches!(m.configurations()[0].len(), 3 | 5)
                && m.configurations()[0].contains(&local)
                && m.nodes().len() == m.configurations()[0].len()
                && observation
                    .committed_membership
                    .as_ref()
                    .is_some_and(|committed| committed.membership == *m)
        });
        if !valid {
            executor.shutdown().await.map_err(authority)?;
            return Err(RuntimeError::UnsupportedMembership);
        }
        let registration = match register(executor.clone()) {
            Ok(registration) => registration,
            Err(error) => {
                executor.shutdown().await.map_err(authority)?;
                return Err(error);
            }
        };
        let fixed = FixedPeers {
            automatic: automatic.then(AutomaticElection::new),
            next_heartbeat: tokio::time::Instant::now(),
            read_turn: false,
            cursor: 0,
            recent: std::collections::VecDeque::new(),
            ticks: 0,
            busy_ticks: 0,
            read: None,
            membership: membership.ok_or(RuntimeError::UnsupportedMembership)?,
            transport: Arc::new(EndpointTransport { caller, local }),
            tasks: JoinSet::new(),
            in_flight: BTreeSet::new(),
            campaign: None,
            pending: BTreeMap::new(),
        };
        let (requests, receive) = mpsc::channel(ADMISSION_CAPACITY);
        let (stop, stopping) = watch::channel(false);
        let (ready, readiness) = oneshot::channel();
        recovering.0 = None;
        let handle = tokio::spawn(async move {
            let result = run(&executor, local, receive, stopping, ready, Some(fixed)).await;
            drop(registration);
            executor.shutdown().await.map_err(authority).and(result)
        });
        let runtime = Self {
            requests,
            stop,
            task: Mutex::new(Task {
                handle: Some(handle),
                result: None,
            }),
        };
        match readiness.await {
            Ok(Ok(())) => Ok(runtime),
            Ok(Err(error)) => {
                let _ = runtime.shutdown().await;
                Err(error)
            }
            Err(_) => {
                runtime.shutdown().await?;
                Err(RuntimeError::Stopped)
            }
        }
    }

    /// Explicit election trigger until accepted-leader activity is exposed by
    /// the store. This does not grant leadership; real signed votes are required.
    async fn campaign(&self) -> Result<(), RuntimeError> {
        let (send, receive) = oneshot::channel();
        self.admit(Request::Campaign(send))?;
        receive.await.map_err(|_| RuntimeError::Stopped)?
    }

    async fn submit<F>(
        &self,
        principal: String,
        command: ControlCommand,
        authorize: F,
    ) -> Result<ControlResponse, RuntimeError>
    where
        F: FnOnce(&ControlState, &ControlCommand) -> Result<(), String> + Send + 'static,
    {
        // Bound retained queue bytes before transferring command ownership.
        // This private qualification entry point is not a new public admission
        // contract and must not be connected to legacy control submission.
        crate::control_codec::control_command_encoded_len(&command, COMMAND_BYTES)
            .map_err(|error| RuntimeError::Rejected(error.to_string()))?;
        let (response, receive) = oneshot::channel();
        self.admit(Request::Submit {
            principal,
            command: Box::new(command),
            authorize: Box::new(authorize),
            response,
        })?;
        tokio::time::timeout(REQUEST_TIMEOUT, receive)
            .await
            .map_err(|_| RuntimeError::Indeterminate)?
            .map_err(|_| RuntimeError::Indeterminate)?
    }

    async fn read_linearizable(&self) -> Result<AuthorizedRead, RuntimeError> {
        let (send, receive) = oneshot::channel();
        self.admit(Request::ReadLinearizable(send))?;
        tokio::time::timeout(REQUEST_TIMEOUT, receive)
            .await
            .map_err(|_| RuntimeError::QuorumUnavailable)?
            .map_err(|_| RuntimeError::Stopped)?
    }

    async fn observe(&self) -> Result<OwnedObservation, RuntimeError> {
        let (response, receive) = oneshot::channel();
        self.admit(Request::Observe(response))?;
        tokio::time::timeout(REQUEST_TIMEOUT, receive)
            .await
            .map_err(|_| RuntimeError::Stopped)?
            .map_err(|_| RuntimeError::Stopped)?
    }

    /// Canonical applied state, explicitly NOT a quorum-confirmed read. The
    /// missing executor-integrated read barrier must not be approximated here.
    async fn read_stale(&self) -> Result<ControlState, RuntimeError> {
        let (response, receive) = oneshot::channel();
        self.admit(Request::ReadStale(response))?;
        tokio::time::timeout(REQUEST_TIMEOUT, receive)
            .await
            .map_err(|_| RuntimeError::Stopped)?
            .map_err(|_| RuntimeError::Stopped)?
    }

    fn admit(&self, request: Request) -> Result<(), RuntimeError> {
        if *self.stop.borrow() {
            return Err(RuntimeError::Stopped);
        }
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RuntimeError::Capacity,
                mpsc::error::TrySendError::Closed(_) => RuntimeError::Stopped,
            })
    }

    /// Cancellation retains the join handle, so another caller can finish
    /// waiting. Repeated shutdown returns the original completion result.
    async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.stop.send_replace(true);
        let mut task = self.task.lock().await;
        if let Some(result) = &task.result {
            return result.clone();
        }
        let result = match task.handle.as_mut() {
            Some(handle) => handle
                .await
                .unwrap_or_else(|error| Err(RuntimeError::Authority(error.to_string()))),
            None => Err(RuntimeError::Stopped),
        };
        task.handle = None;
        task.result = Some(result.clone());
        result
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        // Dropping requests does not authorize dropping an admitted storage
        // operation. The task finishes it and closes the store itself.
        self.stop.send_replace(true);
    }
}

fn recovery_error(error: std::io::Error) -> RuntimeError {
    // The executor reports WouldBlock only for definitive pre-admission
    // capacity/ownership rejection, before returning an executor or registering
    // services. Do not classify uncertain recovery/storage failures as retryable.
    if error.kind() == std::io::ErrorKind::WouldBlock {
        RuntimeError::Capacity
    } else {
        authority(error)
    }
}

fn authority(error: impl std::fmt::Debug) -> RuntimeError {
    RuntimeError::Authority(format!("{error:?}"))
}

fn singleton(membership: Option<&Membership>, local: NodeId) -> bool {
    membership.is_some_and(|membership| {
        membership.configurations().len() == 1
            && membership.configurations()[0].len() == 1
            && membership.configurations()[0].contains(&local)
            && membership.nodes().len() == 1
    })
}

async fn inspect(executor: &VoteExecutor, local: NodeId) -> Result<OwnedObservation, RuntimeError> {
    let observation = executor.observe(local).await.map_err(authority)?;
    if !singleton(
        observation
            .effective_membership
            .as_ref()
            .map(|m| &m.membership),
        local,
    ) || !singleton(
        observation
            .committed_membership
            .as_ref()
            .map(|m| &m.membership),
        local,
    ) {
        return Err(RuntimeError::UnsupportedMembership);
    }
    Ok(observation)
}

/// Drain bounded application turns without using unlabelled responses as a
/// waiter queue. No request can interleave while recovery application runs.
async fn catch_up(executor: &VoteExecutor, local: NodeId) -> Result<(), RuntimeError> {
    for _ in 0..APPLICATION_TURNS {
        let observation = inspect(executor, local).await?;
        if observation.applied == observation.committed {
            return Ok(());
        }
        let batch = executor.apply_committed_batch().await.map_err(authority)?;
        if batch.outcomes.len() > 64 || batch.applied == observation.applied {
            return Err(RuntimeError::Authority(
                "invalid application batch progress".into(),
            ));
        }
        if !batch.more {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
    Err(RuntimeError::Authority(
        "application turn budget exhausted".into(),
    ))
}

async fn compact_applied(executor: &VoteExecutor, local: NodeId) -> Result<(), RuntimeError> {
    let observation = inspect(executor, local).await?;
    if observation.applied.is_none() {
        return Ok(());
    }
    // Store publication captures the canonical applied prefix and membership.
    // Compaction removes only that snapshot boundary; an uncommitted or
    // unapplied suffix remains authoritative and must survive recovery.
    let snapshot = executor.publish_snapshot().await.map_err(authority)?;
    drop(snapshot);
    executor.compact_snapshot().await.map_err(authority)
}

async fn initialize(executor: &VoteExecutor, local: NodeId) -> Result<(), RuntimeError> {
    inspect(executor, local).await?;
    catch_up(executor, local).await?;
    // Recovery must reclaim the applied prefix before appending the election
    // no-op: a cleanly applied but full retained log must remain restartable.
    compact_applied(executor, local).await?;
    executor
        .campaign(local)
        .await
        .map_err(authority)?
        .ok_or_else(|| RuntimeError::Authority("local voter cannot campaign".into()))?;
    executor
        .initialize_leader()
        .await
        .map_err(authority)?
        .ok_or_else(|| RuntimeError::Authority("single-voter campaign has no quorum".into()))?;
    executor.commit_replicated().await.map_err(authority)?;
    catch_up(executor, local).await?;
    let observation = inspect(executor, local).await?;
    if !observation.local_leader
        || observation
            .committed
            .is_none_or(|position| position.term != observation.hard_state.term)
    {
        return Err(RuntimeError::Authority(
            "leader no-op is not committed".into(),
        ));
    }
    Ok(())
}

/// The proposal identity is returned only by successful authorized admission.
/// Keep its authenticated principal and canonical command bound while waiting;
/// equal command IDs from other principals/entries cannot satisfy this waiter.
struct PendingApplication<'a> {
    identity: LogIdentity,
    principal: &'a str,
    command: &'a ControlCommand,
}

async fn apply_proposal(
    executor: &VoteExecutor,
    pending: PendingApplication<'_>,
) -> Result<ControlResponse, RuntimeError> {
    if pending.principal != pending.command.principal_id {
        return Err(RuntimeError::Indeterminate);
    }
    executor
        .commit_replicated()
        .await
        .map_err(|_| RuntimeError::Indeterminate)?;
    for _ in 0..APPLICATION_TURNS {
        let batch = executor
            .apply_committed_batch()
            .await
            .map_err(|_| RuntimeError::Indeterminate)?;
        if batch.outcomes.len() > 64 {
            return Err(RuntimeError::Indeterminate);
        }
        for outcome in batch.outcomes {
            if outcome.identity != pending.identity {
                continue;
            }
            // This exact identity was bound to the authorized canonical command
            // by proposal admission, not inferred from a response command ID.
            let canonical = outcome.response.ok_or(RuntimeError::Indeterminate)?;
            let response = crate::control_codec::decode_control_response(&canonical)
                .map_err(|_| RuntimeError::Indeterminate)?;
            if response.command_id != pending.command.command_id
                || batch
                    .applied
                    .is_none_or(|applied| applied.index < pending.identity.index)
            {
                return Err(RuntimeError::Indeterminate);
            }
            return Ok(response);
        }
        // An applied watermark without the exact tagged identity is not proof
        // of this proposal's outcome (including an overwritten same index).
        if !batch.more
            || batch
                .applied
                .is_some_and(|applied| applied.index >= pending.identity.index)
        {
            return Err(RuntimeError::Indeterminate);
        }
        tokio::task::yield_now().await;
    }
    Err(RuntimeError::Indeterminate)
}

async fn submit(
    executor: &VoteExecutor,
    local: NodeId,
    principal: String,
    command: ControlCommand,
    authorize: Authorize,
) -> Result<ControlResponse, RuntimeError> {
    catch_up(executor, local).await?;
    let observation = inspect(executor, local).await?;
    if !observation.local_leader {
        return Err(RuntimeError::Rejected("local node is not leader".into()));
    }
    let identity = executor
        .propose_authorized(
            local,
            observation.hard_state,
            principal.clone(),
            command.clone(),
            authorize,
        )
        .await
        .map_err(|error| match error {
            ProposalError::InvalidCommand(reason) | ProposalError::Unauthorized(reason) => {
                RuntimeError::Rejected(reason)
            }
            ProposalError::StaleAuthority | ProposalError::NotReady => {
                RuntimeError::Rejected("proposal authority changed before admission".into())
            }
            ProposalError::IndexExhausted => {
                RuntimeError::Authority("log identity exhausted".into())
            }
            ProposalError::Capacity => RuntimeError::Capacity,
            ProposalError::Append(_) | ProposalError::Executor(_) => RuntimeError::Indeterminate,
        })?;
    apply_proposal(
        executor,
        PendingApplication {
            identity,
            principal: &principal,
            command: &command,
        },
    )
    .await
}

fn proposal_error(error: ProposalError) -> RuntimeError {
    match error {
        ProposalError::InvalidCommand(reason) | ProposalError::Unauthorized(reason) => {
            RuntimeError::Rejected(reason)
        }
        ProposalError::StaleAuthority | ProposalError::NotReady => {
            RuntimeError::Rejected("proposal authority changed before admission".into())
        }
        ProposalError::IndexExhausted => RuntimeError::Authority("log identity exhausted".into()),
        ProposalError::Capacity
        | ProposalError::Executor(crate::consensus_model::VoteGateError::Busy) => {
            RuntimeError::Capacity
        }
        ProposalError::Append(_) | ProposalError::Executor(_) => RuntimeError::Indeterminate,
    }
}

fn proposal_key(identity: LogIdentity) -> (u64, NodeId, u64) {
    (identity.term.0, identity.leader, identity.index.0)
}

fn fixed_gate_error(error: crate::consensus_model::VoteGateError) -> RuntimeError {
    match error {
        crate::consensus_model::VoteGateError::Busy => RuntimeError::Capacity,
        other => authority(other),
    }
}

async fn automatic_campaign(
    executor: &VoteExecutor,
    local: NodeId,
    fixed: &mut FixedPeers,
    observation: &OwnedObservation,
) -> Result<(), RuntimeError> {
    let Some(election) = fixed.automatic.as_mut() else {
        return Ok(());
    };
    let activity = observation.election_activity.as_ref();
    let sequence = activity.map(|activity| activity.sequence);
    if let Some(activity) = activity
        && activity.term < observation.hard_state.term
    {
        // A newer local campaign or verified term supersedes old vote/leader
        // evidence. Mark it consumed without shortening the new deadline.
        election.activity_sequence = sequence;
    }
    if let Some(activity) = activity
        && election.observe_activity(*activity)
    {
        // Both a persisted granted vote and accepted same-term leader activity
        // reset the timer. Evidence older than our campaign cannot cancel it.
        if fixed
            .campaign
            .is_some_and(|campaign| activity.term >= campaign.term)
        {
            fixed.campaign = None;
        }
    }
    if observation.local_leader || tokio::time::Instant::now() < election.deadline {
        return Ok(());
    }
    let Some(membership) = observation.effective_membership.clone() else {
        return Err(RuntimeError::UnsupportedMembership);
    };
    let campaign = executor
        .campaign_if_unchanged(local, observation.hard_state, membership, sequence)
        .await
        .map_err(fixed_gate_error)?;
    // Busy or a failed guard must not buy another complete election timeout.
    // Keep the expired deadline until fresh activity or an admitted campaign;
    // the bounded progress tick supplies retry backoff, not a tight loop.
    if let Some(campaign) = campaign {
        election.record(format!(
            "campaign term={:?} activity={sequence:?} age={:?}",
            campaign.term,
            activity.map(|activity| activity.observed_at.elapsed())
        ));
        election.reset(tokio::time::Instant::now());
        fixed.campaign = Some(campaign);
        fixed.next_heartbeat = tokio::time::Instant::now();
    }
    Ok(())
}

fn validate_fixed_membership(
    observation: &OwnedObservation,
    membership: &Membership,
) -> Result<(), RuntimeError> {
    if observation
        .effective_membership
        .as_ref()
        .is_none_or(|m| &m.membership != membership)
        || observation
            .committed_membership
            .as_ref()
            .is_none_or(|m| &m.membership != membership)
    {
        return Err(RuntimeError::UnsupportedMembership);
    }
    Ok(())
}

async fn progress_follower(
    executor: &VoteExecutor,
    fixed: &mut FixedPeers,
    observation: &OwnedObservation,
) -> Result<(), RuntimeError> {
    expire_pending_proposals(fixed, observation);
    if let Some(read) = fixed.read.take() {
        let _ = read.response.send(Err(RuntimeError::QuorumUnavailable));
    }
    if observation.applied != observation.committed {
        executor
            .apply_committed_batch()
            .await
            .map_err(|error| match error {
                crate::owned_vote_store::AppendExecutionError::Executor(
                    crate::consensus_model::VoteGateError::Busy,
                ) => RuntimeError::Capacity,
                other => authority(other),
            })?;
    }
    Ok(())
}

fn idle_leader(fixed: &FixedPeers, observation: &OwnedObservation) -> bool {
    fixed.automatic.is_some()
        && observation.local_leader
        && observation.last_log == observation.committed
        && observation.applied == observation.committed
        && fixed.pending.is_empty()
        && fixed.read.is_none()
}

async fn fixed_progress(
    executor: &Arc<VoteExecutor>,
    local: NodeId,
    fixed: &mut FixedPeers,
) -> Result<(), RuntimeError> {
    if fixed.preparing() {
        return Ok(());
    }
    let observation = executor.observe(local).await.map_err(fixed_gate_error)?;
    validate_fixed_membership(&observation, &fixed.membership)?;
    if idle_leader(fixed, &observation) {
        // No local durability work exists. In particular do not begin an idle
        // commit transaction and four more reads before a heartbeat turn.
        schedule_peers(executor, local, fixed, &observation);
        return Ok(());
    }
    automatic_campaign(executor, local, fixed, &observation).await?;
    // An idle follower needs one coherent observation, not repeated reads of
    // the same authority on every 25ms tick. Those extra blocking admissions
    // contend with the authenticated heartbeat that keeps this node following.
    if fixed.automatic.is_some() && !observation.local_leader && fixed.campaign.is_none() {
        progress_follower(executor, fixed, &observation).await?;
        return Ok(());
    }
    // A successful guarded campaign has durably advanced the term. Do not
    // compare its request against the pre-campaign observation below.
    let observation = executor.observe(local).await.map_err(fixed_gate_error)?;
    if !observation.local_leader {
        // No inferred stepdown: the executor has already persisted any higher
        // authenticated term before publishing this observation.
        if fixed
            .campaign
            .is_some_and(|campaign| campaign.term != observation.hard_state.term)
        {
            fixed.campaign = None;
        }
        if fixed.campaign.is_some() {
            match executor.initialize_leader().await {
                Ok(Some(_)) => fixed.campaign = None,
                Ok(None) | Err(crate::consensus_model::VoteGateError::Busy) => {}
                Err(error) => return Err(authority(error)),
            }
        }
    }
    let observation = executor.observe(local).await.map_err(fixed_gate_error)?;
    if observation.local_leader {
        executor
            .commit_replicated()
            .await
            .map_err(fixed_gate_error)?;
    }
    let observation = executor.observe(local).await.map_err(fixed_gate_error)?;
    // Do not compete with inbound votes/appends by opening a canonical write
    // transaction on every idle follower tick. Apply only real committed work.
    let outcomes = if observation.applied == observation.committed {
        Vec::new()
    } else {
        let batch = executor
            .apply_committed_batch()
            .await
            .map_err(|error| match error {
                crate::owned_vote_store::AppendExecutionError::Executor(
                    crate::consensus_model::VoteGateError::Busy,
                ) => RuntimeError::Capacity,
                other => authority(other),
            })?;
        if batch.outcomes.len() > 64 {
            return Err(RuntimeError::Authority(
                "oversized application batch".into(),
            ));
        }
        batch.outcomes
    };
    for outcome in outcomes {
        if let Some(pending) = fixed.pending.remove(&proposal_key(outcome.identity)) {
            let result = if pending.identity == outcome.identity
                && pending.principal == pending.command.principal_id
            {
                outcome
                    .response
                    .as_deref()
                    .and_then(|bytes| crate::control_codec::decode_control_response(bytes).ok())
                    .filter(|response| response.command_id == pending.command.command_id)
                    .ok_or(RuntimeError::Indeterminate)
            } else {
                Err(RuntimeError::Indeterminate)
            };
            let _ = pending.response.send(result);
        }
    }
    let observation = executor.observe(local).await.map_err(fixed_gate_error)?;
    expire_pending_proposals(fixed, &observation);
    progress_read(executor, local, fixed, &observation).await?;
    schedule_peers(executor, local, fixed, &observation);
    Ok(())
}

fn expire_pending_proposals(fixed: &mut FixedPeers, observation: &OwnedObservation) {
    let now = tokio::time::Instant::now();
    let expired = fixed
        .pending
        .iter()
        .filter_map(|(index, pending)| {
            (pending.deadline <= now
                || pending.identity.term != observation.hard_state.term
                || !observation.local_leader)
                .then_some(*index)
        })
        .collect::<Vec<_>>();
    for index in expired {
        if let Some(pending) = fixed.pending.remove(&index) {
            let _ = pending.response.send(Err(RuntimeError::Indeterminate));
        }
    }
}

async fn progress_read(
    executor: &Arc<VoteExecutor>,
    local: NodeId,
    fixed: &mut FixedPeers,
    observation: &OwnedObservation,
) -> Result<(), RuntimeError> {
    let Some(read) = fixed.read.as_ref() else {
        return Ok(());
    };
    if read.deadline <= tokio::time::Instant::now()
        || read.context.term != observation.hard_state.term
        || !observation.local_leader
        || read.response.is_closed()
    {
        if let Some(read) = fixed.read.take() {
            let _ = read.response.send(Err(RuntimeError::QuorumUnavailable));
        }
        return Ok(());
    }
    match executor.complete_read(local, read.context).await {
        Ok(Some(authorized)) => {
            if let Some(read) = fixed.read.take() {
                let _ = read.response.send(Ok(authorized));
            }
        }
        Ok(None)
        | Err(crate::owned_vote_store::AppendExecutionError::Executor(
            crate::consensus_model::VoteGateError::Busy,
        )) => {}
        Err(error) => return Err(authority(error)),
    }
    Ok(())
}

fn schedule_peers(
    executor: &Arc<VoteExecutor>,
    local: NodeId,
    fixed: &mut FixedPeers,
    observation: &OwnedObservation,
) {
    if fixed.automatic.is_some() {
        schedule_automatic_peers(executor, local, fixed, observation);
        return;
    }
    // Fixed qualification serializes preparation; automatic operation instead
    // bounds each peer independently so a slow RPC cannot stop local progress.
    if !fixed.tasks.is_empty() || !fixed.in_flight.is_empty() {
        return;
    }
    let peers = fixed.membership.configurations()[0]
        .iter()
        .copied()
        .filter(|peer| *peer != local)
        .collect::<Vec<_>>();
    for offset in 0..peers.len() {
        let index = (fixed.cursor + offset) % peers.len();
        let peer = peers[index];
        if fixed.in_flight.contains(&peer) {
            continue;
        }
        let operation = if let Some(read) = fixed
            .read
            .as_mut()
            .filter(|read| !read.attempted.contains(&peer))
        {
            read.attempted.insert(peer);
            fixed
                .transport
                .confirm(executor.clone(), read.context, peer)
        } else if observation.local_leader {
            fixed.transport.replicate(executor.clone(), peer)
        } else if let Some(campaign) = fixed.campaign {
            fixed.transport.vote(executor.clone(), campaign, peer)
        } else {
            continue;
        };
        fixed.cursor = (index + 1) % peers.len();
        fixed.in_flight.insert(peer);
        fixed.tasks.spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(50), operation)
                .await
                .unwrap_or(Err(RpcError::ExecutionFailed));
            (peer, result)
        });
        // The cursor rotates after every admitted turn; completion must be
        // consumed before the next peer can contend for the shared permit.
        break;
    }
}

fn schedule_automatic_peers(
    executor: &Arc<VoteExecutor>,
    local: NodeId,
    fixed: &mut FixedPeers,
    observation: &OwnedObservation,
) {
    let now = tokio::time::Instant::now();
    if now < fixed.next_heartbeat || fixed.tasks.len() >= PEER_TASK_CAPACITY {
        return;
    }
    let peers = fixed.membership.configurations()[0]
        .iter()
        .copied()
        .filter(|peer| *peer != local)
        .collect::<Vec<_>>();
    if peers.is_empty() {
        return;
    }
    // Stagger preparation instead of racing four immediate gate admissions.
    // Network futures remain concurrent: an unavailable peer occupies only its
    // own slot. Four peers receive a turn within 100ms at the 25ms tick.
    fixed.next_heartbeat = now
        + HEARTBEAT_INTERVAL
            / u32::try_from(peers.len()).expect("fixed configuration has at most four peers");
    // Alternate read and replication sweeps. A continuous stream of reads must
    // not prevent catch-up or commit-watermark delivery. Each staggered turn
    // admits at most one operation, bounded by the executor's read permit.
    if fixed.cursor == 0 {
        fixed.read_turn = !fixed.read_turn;
    }
    let confirm_available = fixed.read_turn;
    let start = fixed.cursor;
    for offset in 0..peers.len() {
        if fixed.tasks.len() >= PEER_TASK_CAPACITY {
            break;
        }
        let index = (start + offset) % peers.len();
        let peer = peers[index];
        if fixed.in_flight.contains(&peer) {
            continue;
        }
        let read = fixed.read.as_mut().filter(|read| {
            observation.local_leader && confirm_available && !read.attempted.contains(&peer)
        });
        let operation = if let Some(read) = read {
            read.attempted.insert(peer);
            fixed
                .transport
                .confirm(executor.clone(), read.context, peer)
        } else if observation.local_leader {
            fixed.transport.replicate(executor.clone(), peer)
        } else if let Some(campaign) = fixed.campaign {
            fixed.transport.vote(executor.clone(), campaign, peer)
        } else {
            continue;
        };
        fixed.in_flight.insert(peer);
        fixed.tasks.spawn(async move {
            // Bound waiting without aborting executor-owned admitted persistence.
            // Its handoff/settlement and shutdown retain durable ownership.
            let result = tokio::time::timeout(Duration::from_secs(50), operation)
                .await
                .unwrap_or(Err(RpcError::ExecutionFailed));
            (peer, result)
        });
        fixed.cursor = (index + 1) % peers.len();
        return;
    }
    // Rotate even when every peer was busy; slow peers keep only their own slot
    // and cannot hold healthy peers behind a shared serial network turn.
    fixed.cursor = (start + 1) % peers.len();
}

async fn drain_peer_tasks(
    tasks: &mut JoinSet<(NodeId, Result<(), RpcError>)>,
) -> Result<(), RuntimeError> {
    let mut failure = None;
    while let Some(completion) = tasks.join_next().await {
        match completion {
            Err(error) => {
                failure.get_or_insert_with(|| authority(error));
            }
            Ok((_, Err(RpcError::StorageFailure | RpcError::Stopped))) => {
                failure.get_or_insert_with(|| {
                    RuntimeError::Authority("peer operation failed during shutdown".into())
                });
            }
            Ok(_) => {}
        }
    }
    failure.map_or(Ok(()), Err)
}

// Keep admission, select branches, and joined cleanup in one lifecycle state
// machine so cancellation cannot accidentally transfer store ownership.
#[allow(clippy::too_many_lines)]
async fn run(
    executor: &Arc<VoteExecutor>,
    local: NodeId,
    mut requests: mpsc::Receiver<Request>,
    mut stop: watch::Receiver<bool>,
    ready: oneshot::Sender<Result<(), RuntimeError>>,
    mut fixed: Option<FixedPeers>,
) -> Result<(), RuntimeError> {
    let initialized = if fixed.is_some() {
        Ok(())
    } else {
        initialize(executor, local).await
    };
    let started = initialized.is_ok();
    if ready.send(initialized.clone()).is_err() || !started {
        return initialized;
    }
    // Explicit timer events drive bounded local progress and automatic election
    // deadlines. Heartbeat success still comes only from authenticated RPCs.
    // The legacy explicit-campaign qualification scheduler serializes complete
    // peer turns and rejects requests during preparation. Preserve its original
    // 100ms cadence rather than applying the automatic actor's 25ms polling to
    // that serialized admission policy and starving pre-proposal requests.
    let progress_interval = if fixed
        .as_ref()
        .is_some_and(|peers| peers.automatic.is_some())
    {
        PROGRESS_INTERVAL
    } else {
        Duration::from_millis(100)
    };
    let mut timer = tokio::time::interval(progress_interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut result = loop {
        if *stop.borrow() {
            break Ok(());
        }
        tokio::select! {
            _ = stop.changed() => break Ok(()),
            request = requests.recv() => {
                let Some(request) = request else { break Ok(()); };
                // A spawned peer turn owns its gate-admission opportunity until
                // completion. Polling clients must not starve that preparation.
                if fixed.as_ref().is_some_and(FixedPeers::preparing) {
                    match request {
                        Request::Diagnostics(response) => {
                            let _ = response.send(format!("peer preparation active: {:?}", fixed.as_ref().map(|fixed| (&fixed.in_flight, &fixed.recent, fixed.automatic.as_ref().map(|election| &election.history)))));
                        }
                        Request::ReadLinearizable(response) => { let _ = response.send(Err(RuntimeError::Capacity)); }
                        Request::Campaign(response) => { let _ = response.send(Err(RuntimeError::Capacity)); }
                        Request::Submit { response, .. } => { let _ = response.send(Err(RuntimeError::Capacity)); }
                        Request::Observe(response) => { let _ = response.send(Err(RuntimeError::Capacity)); }
                        Request::ReadStale(response) => { let _ = response.send(Err(RuntimeError::Capacity)); }
                    }
                    continue;
                }
                match request {
                    Request::Diagnostics(response) => {
                        let diagnostic = fixed.as_ref().map_or_else(|| "single-voter".into(), |fixed| format!(
                            "ticks={} busy_ticks={} campaign={:?} in_flight={:?} tasks={} pending={:?} read={:?} recent={:?} election={:?}",
                            fixed.ticks, fixed.busy_ticks, fixed.campaign, fixed.in_flight, fixed.tasks.len(), fixed.pending.keys().collect::<Vec<_>>(),
                            fixed.read.as_ref().map(|read| (read.context, &read.attempted, read.deadline.saturating_duration_since(tokio::time::Instant::now()))), fixed.recent,
                            fixed.automatic.as_ref().map(|election| (&election.history, election.activity_sequence, election.deadline.saturating_duration_since(tokio::time::Instant::now()))),
                        ));
                        let _ = response.send(diagnostic);
                    }
                    Request::ReadLinearizable(response) => {
                        match fixed.as_mut() {
                            Some(fixed) if fixed.read.is_none() => {
                                match executor.begin_read(local).await {
                                    Ok(Some(context)) => fixed.read = Some(PendingRead { context, response, deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT, attempted: BTreeSet::new() }),
                                    Ok(None) => { let _ = response.send(Err(RuntimeError::QuorumUnavailable)); }
                                    Err(error) => { let _ = response.send(Err(fixed_gate_error(error))); }
                                }
                            }
                            Some(_) => { let _ = response.send(Err(RuntimeError::Capacity)); }
                            None => { let _ = response.send(Err(RuntimeError::UnsupportedMembership)); }
                        }
                    }
                    Request::Campaign(response) => {
                        let result = match fixed.as_mut() {
                            Some(fixed) if fixed.tasks.is_empty() => {
                                executor.campaign(local).await.map_err(authority).map(|campaign| { fixed.campaign = campaign; })
                            }
                            Some(_) => Err(RuntimeError::Capacity),
                            None => Err(RuntimeError::UnsupportedMembership),
                        };
                        let _ = response.send(result);
                    }
                    Request::Submit { principal, command, authorize, response } => {
                        if let Some(fixed) = fixed.as_mut() {
                            if fixed.pending.len() >= ADMISSION_CAPACITY {
                                let _ = response.send(Err(RuntimeError::Capacity));
                                continue;
                            }
                            let observation = match executor.observe(local).await {
                                Ok(observation) => observation,
                                Err(crate::consensus_model::VoteGateError::Busy) => { let _ = response.send(Err(RuntimeError::Capacity)); continue; }
                                Err(error) => { let error = authority(error); let _ = response.send(Err(error.clone())); break Err(error); }
                            };
                            match executor.propose_authorized(local, observation.hard_state, principal.clone(), (*command).clone(), authorize).await {
                                Ok(identity) => {
                                    fixed.next_heartbeat = tokio::time::Instant::now();
                                    // Proposal admission is already durable. Wake
                                    // replication now rather than waiting for an
                                    // idle heartbeat; executor fencing rechecks
                                    // the observation before any actual send.
                                    if fixed.automatic.is_some() {
                                        schedule_peers(executor, local, fixed, &observation);
                                    }
                                    fixed.pending.insert(proposal_key(identity), PendingProposal { identity, principal, command, response, deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT });
                                }
                                Err(error) => { let _ = response.send(Err(proposal_error(error))); }
                            }
                            continue;
                        }
                        // Once admitted, caller cancellation stops waiting only.
                        let large = crate::control_codec::control_command_encoded_len(&command, COMMAND_BYTES)
                            .is_ok_and(|bytes| bytes >= LARGE_COMMAND_BYTES);
                        let result = submit(executor, local, principal, *command, authorize).await;
                        let fatal = match &result {
                            Err(RuntimeError::Rejected(_) | RuntimeError::Capacity) | Ok(_) => None,
                            Err(error) => Some(error.clone()),
                        };
                        let succeeded = result.is_ok();
                        let capacity = matches!(result, Err(RuntimeError::Capacity));
                        // Capacity is a proven pre-append rejection. Finish
                        // reclaiming applied log bytes before returning it;
                        // the next explicit caller retry cannot race cleanup.
                        if capacity
                            && let Err(error) = compact_applied(executor, local).await
                        {
                            let _ = response.send(Err(error.clone()));
                            break Err(error);
                        }
                        let _ = response.send(result);
                        if let Some(error) = fatal { break Err(error); }
                        if succeeded {
                            match inspect(executor, local).await {
                                Ok(observation) if large || observation.applied.is_some_and(|position| position.index.0.is_multiple_of(COMPACTION_INTERVAL)) => {
                                    if let Err(error) = compact_applied(executor, local).await { break Err(error); }
                                }
                                Ok(_) => {}
                                Err(error) => break Err(error),
                            }
                        }
                    }
                    Request::Observe(response) => {
                        let result = if fixed.is_some() { executor.observe(local).await.map_err(fixed_gate_error) } else { inspect(executor, local).await };
                        let failure = result.as_ref().err().filter(|error| !matches!(error, RuntimeError::Capacity)).cloned();
                        let _ = response.send(result);
                        if let Some(error) = failure { break Err(error); }
                    }
                    Request::ReadStale(response) => {
                        let result = executor.read_control_state().await.map_err(|error| match error {
                            crate::owned_vote_store::AppendExecutionError::Executor(crate::consensus_model::VoteGateError::Busy) => RuntimeError::Capacity,
                            other => authority(other),
                        });
                        let failure = result.as_ref().err().filter(|error| !matches!(error, RuntimeError::Capacity)).cloned();
                        let _ = response.send(result);
                        if let Some(error) = failure { break Err(error); }
                    }
                }
            }
            completion = async {
                match fixed.as_mut() {
                    Some(fixed) => fixed.tasks.join_next().await,
                    None => std::future::pending().await,
                }
            }, if fixed.as_ref().is_some_and(|fixed| !fixed.tasks.is_empty()) => {
                match completion {
                    Some(Ok((peer, result))) => {
                        if let Some(fixed) = fixed.as_mut() { fixed.completed(peer, result); }
                        if let Err(error) = result {
                            // A remote storage failure is not evidence that
                            // this owner's durable authority failed. Local
                            // errors are rechecked through observe/progress.
                            tracing::debug!(?peer, ?error, "owned peer turn did not complete");
                        }
                    }
                    Some(Err(error)) => break Err(authority(error)),
                    None => {}
                }
            }
            _ = timer.tick() => {
                if let Some(fixed) = fixed.as_mut() {
                    fixed.expire_waiters(tokio::time::Instant::now());
                    fixed.ticks = fixed.ticks.saturating_add(1);
                    match fixed_progress(executor, local, fixed).await {
                        Ok(()) => {}
                        Err(RuntimeError::Capacity) => { fixed.busy_ticks = fixed.busy_ticks.saturating_add(1); }
                        Err(error) => break Err(error),
                    }
                } else if let Err(error) = catch_up(executor, local).await { break Err(error); }
            }
        }
    };
    requests.close();
    if let Some(fixed) = fixed.as_mut() {
        // Each peer future owns at most one bounded generated RPC turn. Do not
        // abort a persistence handoff: join all admitted work before store close.
        if let Err(error) = drain_peer_tasks(&mut fixed.tasks).await {
            result = Err(error);
        }
        if let Some(read) = fixed.read.take() {
            let _ = read.response.send(Err(RuntimeError::Stopped));
        }
        for (_, pending) in std::mem::take(&mut fixed.pending) {
            let _ = pending.response.send(Err(RuntimeError::Indeterminate));
        }
    }
    // Queued submissions have not entered proposal admission. Be conservative
    // about their externally visible outcome and never replay them on shutdown.
    while let Ok(request) = requests.try_recv() {
        match request {
            Request::Diagnostics(response) => {
                let _ = response.send("stopped".into());
            }
            Request::ReadLinearizable(response) => {
                let _ = response.send(Err(RuntimeError::Stopped));
            }
            Request::Campaign(response) => {
                let _ = response.send(Err(RuntimeError::Stopped));
            }
            Request::Submit { response, .. } => {
                let _ = response.send(Err(RuntimeError::Indeterminate));
            }
            Request::Observe(response) => {
                let _ = response.send(Err(RuntimeError::Stopped));
            }
            Request::ReadStale(response) => {
                let _ = response.send(Err(RuntimeError::Stopped));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owned_vote_store::OwnedVoteStore;
    use bmux_cluster_plugin_api::cluster_types::{CommandId, ControlCommandRequest, WorkspaceId};

    // Recovery deliberately uses immediate bounded admission. Do not have these
    // lifecycle scenarios race each other for the process-wide recovery permit.
    static RECOVERY_SCENARIO: Mutex<()> = Mutex::const_new(());

    fn command(id: u128) -> ControlCommand {
        ControlCommand {
            schema_version: 1,
            principal_id: "principal:test".into(),
            command_id: CommandId {
                value: uuid::Uuid::from_u128(id),
            },
            issued_at_unix_ms: 1,
            request: ControlCommandRequest::CreateWorkspace {
                workspace_id: WorkspaceId {
                    value: uuid::Uuid::from_u128(1),
                },
                name: Some("owned".into()),
            },
        }
    }

    fn bootstrap(path: &std::path::Path, local: NodeId) {
        let membership = Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap();
        OwnedVoteStore::bootstrap(path, "owned-runtime", local, membership)
            .unwrap()
            .close()
            .unwrap();
    }

    #[tokio::test]
    async fn real_single_voter_durable_response_dedup_and_restart() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let runtime = OwnedRuntime::recover(path.clone(), "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        let initial = runtime.observe().await.unwrap();
        assert!(initial.local_leader);
        assert_eq!(initial.applied, initial.committed);
        let response = runtime
            .submit("principal:test".into(), command(1), |_, _| Ok(()))
            .await
            .unwrap();
        let duplicate = runtime
            .submit("principal:test".into(), command(1), |_, _| Ok(()))
            .await
            .unwrap();
        assert_eq!(response, duplicate);
        assert_eq!(
            runtime.read_stale().await.unwrap().revision,
            response.control_revision
        );
        runtime.shutdown().await.unwrap();
        runtime.shutdown().await.unwrap();
        assert_eq!(runtime.observe().await.unwrap_err(), RuntimeError::Stopped);
        let recovered = OwnedRuntime::recover(path, "owned-runtime".into(), local, 2)
            .await
            .unwrap();
        assert!(recovered.observe().await.unwrap().hard_state.term > initial.hard_state.term);
        let retry = recovered
            .submit("principal:test".into(), command(1), |_, _| Ok(()))
            .await
            .unwrap();
        assert_eq!(retry, response);
        recovered.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn denied_authorization_does_not_append() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let runtime = OwnedRuntime::recover(path, "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        let before = runtime.observe().await.unwrap();
        assert!(matches!(
            runtime
                .submit("principal:test".into(), command(1), |_, _| Err(
                    "denied".into()
                ))
                .await,
            Err(RuntimeError::Rejected(_))
        ));
        assert_eq!(runtime.observe().await.unwrap().last_log, before.last_log);
        assert!(matches!(
            runtime
                .submit("different-principal".into(), command(1), |_, _| Ok(()))
                .await,
            Err(RuntimeError::Rejected(_))
        ));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_admission_and_cancelled_waiter_preserve_durable_intent() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let runtime = OwnedRuntime::recover(path, "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        // On this current-thread runtime no receiver runs until we yield. Test
        // the actual mailbox admission, not a synthetic successful executor.
        let mut waiting = Vec::new();
        for _ in 0..ADMISSION_CAPACITY {
            let (send, receive) = oneshot::channel();
            runtime.admit(Request::Observe(send)).unwrap();
            waiting.push(receive);
        }
        let (send, _) = oneshot::channel();
        assert_eq!(
            runtime.admit(Request::Observe(send)),
            Err(RuntimeError::Capacity)
        );
        for receive in waiting {
            receive.await.unwrap().unwrap();
        }
        let (response, receive) = oneshot::channel();
        runtime
            .admit(Request::Submit {
                principal: "principal:test".into(),
                command: Box::new(command(1)),
                authorize: Box::new(|_, _| Ok(())),
                response,
            })
            .unwrap();
        drop(receive);
        // FIFO observation waits behind the real admitted proposal even though
        // its response receiver no longer exists.
        let observation = runtime.observe().await.unwrap();
        assert_eq!(observation.applied, observation.committed);
        assert_eq!(runtime.read_stale().await.unwrap().revision, 1);
        let mut conflicting = command(1);
        conflicting.request = ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId {
                value: uuid::Uuid::from_u128(2),
            },
            name: None,
        };
        let conflict = runtime
            .submit("principal:test".into(), conflicting, |_, _| Ok(()))
            .await
            .unwrap();
        assert!(matches!(
            conflict.result,
            bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Rejected {
                error:
                    bmux_cluster_plugin_api::cluster_types::ControlCommandError::CommandIdConflict
            }
        ));
        assert_eq!(runtime.read_stale().await.unwrap().revision, 1);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn peer_configuration_is_not_treated_as_single_voter_qualification() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        let peer = NodeId::from(2);
        let membership = Membership::new(
            vec![vec![local]],
            vec![(local, String::new()), (peer, String::new())],
        )
        .unwrap();
        OwnedVoteStore::bootstrap(&path, "owned-runtime", local, membership)
            .unwrap()
            .close()
            .unwrap();
        let result = OwnedRuntime::recover(path.clone(), "owned-runtime".into(), local, 1).await;
        assert!(matches!(result, Err(RuntimeError::UnsupportedMembership)));
        OwnedVoteStore::open(&path, "owned-runtime")
            .unwrap()
            .close()
            .unwrap();
    }

    #[tokio::test]
    async fn full_retained_log_compacts_before_startup_noop_and_keeps_suffix() {
        use crate::consensus_model::{AppendRequest, Entry, EntryPayload, LogIndex, Term};

        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let mut store = OwnedVoteStore::open(&path, "owned-runtime").unwrap();
        let identity = |index| LogIdentity {
            term: Term(0),
            leader: local,
            index: LogIndex(index),
        };
        // Retain exactly the 4096-entry budget, including the founding entry.
        // The final entry is deliberately uncommitted: prefix compaction must
        // not delete it to make room for the next term's no-op.
        let decision = store
            .append(
                &AppendRequest {
                    term: Term(0),
                    leader: local,
                    previous: Some(identity(0)),
                    entries: (1..4096)
                        .map(|index| Entry {
                            identity: identity(index),
                            payload: EntryPayload::NoOp,
                        })
                        .collect(),
                    leader_commit: Some(identity(4094)),
                },
                (4095, 1_048_576),
            )
            .unwrap();
        assert!(decision.outcome.is_ok());
        store.close().unwrap();
        let runtime = OwnedRuntime::recover(path, "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        let observation = runtime.observe().await.unwrap();
        assert_eq!(observation.applied.unwrap().index, LogIndex(4096));
        assert_eq!(observation.applied, observation.last_log);
        assert_eq!(observation.applied, observation.committed);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn byte_capacity_rejection_compacts_without_replaying_proposal() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let executor = VoteExecutor::recover(path.clone(), "owned-runtime".into(), 1)
            .await
            .unwrap();
        initialize(&executor, local).await.unwrap();
        let mut large = command(1);
        large.request = ControlCommandRequest::CompleteWorkflow {
            original_command_id: CommandId {
                value: uuid::Uuid::from_u128(99),
            },
            response: vec![0x5a; 900 * 1024],
        };
        let mut exhausted = false;
        for _ in 0..64 {
            let before = executor.observe(local).await.unwrap();
            match submit(
                &executor,
                local,
                "principal:test".into(),
                large.clone(),
                Box::new(|_, _| Ok(())),
            )
            .await
            {
                Ok(_) => {}
                Err(RuntimeError::Capacity) => {
                    assert_eq!(executor.observe(local).await.unwrap(), before);
                    exhausted = true;
                    break;
                }
                Err(error) => panic!("unexpected pressure outcome: {error:?}"),
            }
        }
        assert!(
            exhausted,
            "test did not exercise actual byte admission pressure"
        );
        // Exercise the same applied-prefix compaction primitive without startup
        // maintenance or proactive large-entry compaction masking the pressure.
        compact_applied(&executor, local).await.unwrap();
        let result = submit(
            &executor,
            local,
            "principal:test".into(),
            large,
            Box::new(|_, _| Ok(())),
        )
        .await;
        assert!(
            result.is_ok(),
            "applied-prefix compaction did not restore capacity: {result:?}"
        );
        executor.shutdown().await.unwrap();
        let recovered = OwnedRuntime::recover(path, "owned-runtime".into(), local, 2)
            .await
            .unwrap();
        recovered.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn large_command_retries_cross_byte_budget_and_restart() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let mut large = command(1);
        large.request = ControlCommandRequest::CompleteWorkflow {
            original_command_id: CommandId {
                value: uuid::Uuid::from_u128(99),
            },
            response: vec![0x5a; 900 * 1024],
        };
        let encoded =
            crate::control_codec::control_command_encoded_len(&large, COMMAND_BYTES).unwrap();
        assert!(encoded > 900 * 1024);
        let runtime = OwnedRuntime::recover(path.clone(), "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        let original = runtime
            .submit("principal:test".into(), large.clone(), |_, _| Ok(()))
            .await
            .unwrap();
        // More than the 32 MiB conservative allocation admission ceiling in
        // cumulative entry bytes, while canonical state retains one outcome.
        // Every large applied entry must trigger prefix compaction, not wait
        // for the 128-entry cadence. No uncertain result is retried here.
        for _ in 0..48 {
            let result = runtime
                .submit("principal:test".into(), large.clone(), |_, _| Ok(()))
                .await;
            let response = match result {
                Ok(response) => response,
                Err(RuntimeError::Capacity) => {
                    // Only proven pre-append capacity permits this one explicit
                    // retry; the runtime completed compaction before replying.
                    runtime
                        .submit("principal:test".into(), large.clone(), |_, _| Ok(()))
                        .await
                        .unwrap()
                }
                Err(error) => panic!("large retry had uncertain or fatal outcome: {error:?}"),
            };
            assert_eq!(response, original);
        }
        let before = runtime.observe().await.unwrap();
        assert_eq!(before.applied, before.committed);
        runtime.shutdown().await.unwrap();
        let recovered = OwnedRuntime::recover(path, "owned-runtime".into(), local, 2)
            .await
            .unwrap();
        assert!(recovered.observe().await.unwrap().hard_state.term > before.hard_state.term);
        assert_eq!(
            recovered
                .submit("principal:test".into(), large, |_, _| Ok(()))
                .await
                .unwrap(),
            original
        );
        recovered.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn retention_boundary_retries_and_restart_preserve_outcome() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let runtime = OwnedRuntime::recover(path.clone(), "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        let original = runtime
            .submit("principal:test".into(), command(1), |_, _| Ok(()))
            .await
            .unwrap();
        // Exceeds the store's 4096-entry retained-log budget. Until an
        // authorized dedup lookup exists, retries still append canonical
        // commands, so periodic snapshot compaction must keep them restartable.
        for _ in 0..4100 {
            let retry = runtime
                .submit("principal:test".into(), command(1), |_, _| Ok(()))
                .await
                .unwrap();
            assert_eq!(retry, original);
        }
        runtime.shutdown().await.unwrap();
        let recovered = OwnedRuntime::recover(path, "owned-runtime".into(), local, 2)
            .await
            .unwrap();
        assert_eq!(
            recovered
                .submit("principal:test".into(), command(1), |_, _| Ok(()))
                .await
                .unwrap(),
            original
        );
        recovered.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_and_concurrent_shutdown_keep_join_ownership() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let runtime = OwnedRuntime::recover(path.clone(), "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        // Queue admission is closed before waiting on the owned task. Dropping
        // this polled shutdown future must not consume its join handle.
        let mut shutdown = Box::pin(runtime.shutdown());
        std::future::poll_fn(|cx| {
            let _ = std::future::Future::poll(shutdown.as_mut(), cx);
            std::task::Poll::Ready(())
        })
        .await;
        drop(shutdown);
        let (left, right) = tokio::join!(runtime.shutdown(), runtime.shutdown());
        left.unwrap();
        right.unwrap();
        assert_eq!(
            runtime
                .submit("principal:test".into(), command(1), |_, _| Ok(()))
                .await,
            Err(RuntimeError::Stopped)
        );
        OwnedVoteStore::open(&path, "owned-runtime")
            .unwrap()
            .close()
            .unwrap();
    }

    #[tokio::test]
    async fn drop_with_queued_admission_joins_storage_before_reopen() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        let runtime = OwnedRuntime::recover(path.clone(), "owned-runtime".into(), local, 1)
            .await
            .unwrap();
        let (response, receive) = oneshot::channel();
        runtime
            .admit(Request::Submit {
                principal: "principal:test".into(),
                command: Box::new(command(1)),
                authorize: Box::new(|_, _| Ok(())),
                response,
            })
            .unwrap();
        // Retain the actual task handle for this drop scenario, not a sleep or
        // guessed lock-release observation. Production Drop still signals stop.
        let handle = runtime.task.lock().await.handle.take().unwrap();
        drop(runtime);
        let outcome = receive.await.unwrap();
        assert!(matches!(outcome, Ok(_) | Err(RuntimeError::Indeterminate)));
        handle.await.unwrap().unwrap();
        let recovered = OwnedRuntime::recover(path, "owned-runtime".into(), local, 2)
            .await
            .unwrap();
        let state = recovered.read_stale().await.unwrap();
        if let Ok(response) = &outcome {
            assert_eq!(response.command_id, command(1).command_id);
            assert!(matches!(
                response.result,
                bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Accepted { .. }
            ));
            assert_eq!(response.control_revision, 1);
            assert_eq!(state.revision, response.control_revision);
            assert_eq!(state.workspaces.len(), 1);
        } else {
            // Indeterminate is not proof of nonapplication. Inspect canonical
            // recovered state rather than assuming the request remained queued.
            assert!(state.revision <= 1);
            assert_eq!(
                state.workspaces.len(),
                usize::try_from(state.revision).unwrap()
            );
        }
        let replay = recovered
            .submit("principal:test".into(), command(1), |_, _| Ok(()))
            .await
            .unwrap();
        if let Ok(response) = outcome {
            assert_eq!(replay, response);
        }
        assert_eq!(replay.control_revision, 1);
        let state = recovered.read_stale().await.unwrap();
        assert_eq!(state.revision, 1);
        assert_eq!(state.workspaces.len(), 1);
        recovered.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_startup_does_not_leave_an_unjoined_store_owner() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        let local = NodeId::from(1);
        bootstrap(&path, local);
        // Cancel recovery before its blocking-worker handoff is acknowledged.
        let mut startup = Box::pin(OwnedRuntime::recover(
            path.clone(),
            "owned-runtime".into(),
            local,
            1,
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(startup.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(startup);
        // Cancellation cleanup runs on the bounded recovery worker. Retry only
        // the documented capacity condition, never reinterpret storage errors.
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                match VoteExecutor::recover(path.clone(), "owned-runtime".into(), 2).await {
                    Ok(executor) => {
                        executor.shutdown().await.unwrap();
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => panic!("unexpected recovery failure: {error}"),
                }
            }
        })
        .await
        .unwrap();
    }

    fn mixed_application_executor(path: &std::path::Path) -> (VoteExecutor, ControlCommand) {
        use crate::consensus_model::{AppendRequest, Entry, EntryPayload, LogIndex, Term};
        use crate::owned_vote_store::DurableVoteGate;

        let local = NodeId::from(1);
        bootstrap(path, local);
        let mut store = OwnedVoteStore::open(path, "owned-runtime").unwrap();
        let identity = |index| LogIdentity {
            term: Term(0),
            leader: local,
            index: LogIndex(index),
        };
        let target = command(1);
        let mut foreign = command(1);
        foreign.principal_id = "principal:other".into();
        let membership = Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap();
        let entries = (1..=70)
            .map(|index| Entry {
                identity: identity(index),
                payload: match index {
                    2 => EntryPayload::Application(crate::control_codec::encode_control_command(
                        &foreign,
                    )),
                    20 => EntryPayload::Membership(membership.clone()),
                    68 => EntryPayload::Application(crate::control_codec::encode_control_command(
                        &target,
                    )),
                    _ => EntryPayload::NoOp,
                },
            })
            .collect();
        assert!(
            store
                .append(
                    &AppendRequest {
                        term: Term(0),
                        leader: local,
                        previous: Some(identity(0)),
                        entries,
                        leader_commit: Some(identity(70)),
                    },
                    (70, COMMAND_BYTES)
                )
                .unwrap()
                .outcome
                .is_ok()
        );
        // A follower's real committed suffix contains a foreign principal with
        // the same command ID, a membership entry and no-ops around the target.
        // Application must cross the 64-entry turn boundary before finding it.
        (VoteExecutor::new(DurableVoteGate::new(1, store)), target)
    }

    #[tokio::test]
    async fn tagged_application_skips_mixed_entries_across_bounded_batches() {
        use crate::consensus_model::{LogIndex, Term};

        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let local = NodeId::from(1);
        let (executor, target) = mixed_application_executor(&root.path().join("owned.redb"));
        let identity = |index| LogIdentity {
            term: Term(0),
            leader: local,
            index: LogIndex(index),
        };
        // A follower's real committed suffix contains a foreign principal with
        // the same command ID, a membership entry and no-ops around the target.
        // A wrong full identity at the right index must not consume either
        // batch's matching command ID as proof of this proposal.
        let (wrong_executor, wrong_target) =
            mixed_application_executor(&root.path().join("wrong.redb"));
        assert_eq!(
            apply_proposal(
                &wrong_executor,
                PendingApplication {
                    identity: LogIdentity {
                        term: Term(1),
                        ..identity(68)
                    },
                    principal: &wrong_target.principal_id,
                    command: &wrong_target,
                }
            )
            .await,
            Err(RuntimeError::Indeterminate)
        );
        wrong_executor.shutdown().await.unwrap();
        let response = apply_proposal(
            &executor,
            PendingApplication {
                identity: identity(68),
                principal: &target.principal_id,
                command: &target,
            },
        )
        .await
        .unwrap();
        assert_eq!(response.command_id, target.command_id);
        assert!(matches!(
            response.result,
            bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Rejected { .. }
        ));
        let observation = executor.observe(local).await.unwrap();
        assert!(!observation.local_leader);
        assert_eq!(observation.applied, Some(identity(70)));
        assert_eq!(observation.applied, observation.committed);
        // A different full identity at the same index cannot be recovered from
        // a watermark or another entry's response, even with the same command.
        assert_eq!(
            apply_proposal(
                &executor,
                PendingApplication {
                    identity: LogIdentity {
                        term: Term(1),
                        ..identity(68)
                    },
                    principal: &target.principal_id,
                    command: &target,
                }
            )
            .await,
            Err(RuntimeError::Indeterminate)
        );
        assert_eq!(
            apply_proposal(
                &executor,
                PendingApplication {
                    identity: identity(68),
                    principal: "principal:other",
                    command: &target,
                }
            )
            .await,
            Err(RuntimeError::Indeterminate)
        );
        executor.shutdown().await.unwrap();
    }

    #[derive(Default)]
    struct TestEndpointNetwork {
        nodes: std::sync::Mutex<BTreeMap<String, Arc<TestEndpointCaller>>>,
        blocked: std::sync::Mutex<BTreeSet<(String, String)>>,
        delays: std::sync::Mutex<BTreeMap<String, Duration>>,
        trace: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl TestEndpointNetwork {
        fn record(&self, event: String) {
            let mut trace = self.trace.lock().unwrap();
            if trace.len() == 32 {
                trace.pop_front();
            }
            trace.push_back(event);
        }
    }

    struct TestEndpointCaller {
        endpoint: String,
        network: Arc<TestEndpointNetwork>,
        storage: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
        executor: std::sync::Mutex<Option<Arc<VoteExecutor>>>,
        scope: crate::durable_storage::TestScope,
    }

    impl bmux_plugin::ServiceCaller for TestEndpointCaller {
        fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
            Some(&self.scope.connection)
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "owned test has no kernel bridge",
            })
        }

        // Dispatch the three generated service envelopes together so this test
        // adapter cannot silently substitute a different authentication path.
        #[allow(clippy::too_many_lines)]
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: bmux_plugin_sdk::ServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            use bmux_connections_plugin_api::{
                connection_types::ConnectionError, connections_commands,
            };
            use bmux_plugin_sdk::{PluginError, decode_service_message, encode_service_message};
            let unsupported = || PluginError::UnsupportedHostOperation {
                operation: "owned test endpoint dispatch",
            };
            match (interface, operation) {
                ("storage-query/v1", "get") => {
                    let request: bmux_plugin_sdk::StorageGetRequest =
                        decode_service_message(&payload)?;
                    return encode_service_message(&bmux_plugin_sdk::StorageGetResponse {
                        value: self
                            .storage
                            .lock()
                            .unwrap()
                            .get(request.key.as_str())
                            .cloned(),
                    });
                }
                ("storage-command/v1", "set") => {
                    let request: bmux_plugin_sdk::StorageSetRequest =
                        decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .unwrap()
                        .insert(request.key.as_str().into(), request.value);
                    return encode_service_message(&());
                }
                _ => {}
            }
            if interface != connections_commands::INTERFACE_ID.as_str()
                || operation != connections_commands::OP_INVOKE_SERVICE.as_str()
            {
                return Err(unsupported());
            }
            let request: connections_commands::client::InvokeServiceRequest =
                decode_service_message(bmux_plugin_sdk::service_contract::decode_for(
                    interface,
                    connections_commands::SERVICE_CONTRACT,
                    &payload,
                )?)?;
            let invocation = request.invocation;
            if self
                .network
                .blocked
                .lock()
                .unwrap()
                .contains(&(self.endpoint.clone(), invocation.target.clone()))
            {
                return Err(unsupported());
            }
            let delay = self
                .network
                .delays
                .lock()
                .unwrap()
                .get(&invocation.target)
                .copied();
            if let Some(delay) = delay {
                // Simulate a slow authenticated endpoint transport without
                // blocking an async runtime worker or holding a network lock.
                tokio::task::block_in_place(|| std::thread::sleep(delay));
            }
            let remote = self
                .network
                .nodes
                .lock()
                .unwrap()
                .get(&invocation.target)
                .cloned()
                .ok_or_else(unsupported)?;
            let executor = remote
                .executor
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(unsupported)?;
            let provider = crate::consensus_network::OwnedAppendService {
                caller: remote.clone(),
                local: *crate::load_or_create_node_identity(remote.as_ref())
                    .unwrap()
                    .node_id(),
                executor: executor.clone(),
            };
            let mut registry = bmux_plugin_sdk::TypedServiceRegistry::new();
            crate::consensus_network::OwnedAppendService::register(
                &mut registry,
                remote.clone(),
                provider.local,
                executor.clone(),
            )
            .map_err(|_| unsupported())?;
            crate::consensus_network::OwnedAppendService::register_append_v2(
                &mut registry,
                remote.clone(),
                provider.local,
                executor.clone(),
            )
            .map_err(|_| unsupported())?;
            let runtime = tokio::runtime::Handle::current();
            let result = match (
                invocation.interface_id.as_str(),
                invocation.operation.as_str(),
            ) {
                ("cluster-peer-auth/v1", "challenge") => {
                    let request: bmux_cluster_plugin_api::cluster_peer_auth::client::ChallengeRequest = decode_service_message(&invocation.payload)?;
                    let state = tokio::task::block_in_place(|| {
                        runtime.block_on(async {
                            // A canonical read rejected before admission is
                            // retryable. Do not turn transient gate contention
                            // into a false peer-authentication failure.
                            for attempt in 0..4 {
                                match executor.read_control_state().await {
                                    Err(
                                        crate::owned_vote_store::AppendExecutionError::Executor(
                                            crate::consensus_model::VoteGateError::Busy,
                                        ),
                                    ) if attempt < 3 => {
                                        tokio::time::sleep(Duration::from_millis(1)).await;
                                    }
                                    result => return result,
                                }
                            }
                            unreachable!("bounded challenge read always returns")
                        })
                    })
                    .map_err(|error| {
                        self.network.record(format!(
                            "auth {} -> {} canonical read failed: {error:?}",
                            self.endpoint, remote.endpoint
                        ));
                        unsupported()
                    })?;
                    let challenge = crate::membership::create_peer_auth_challenge_for_members(
                        remote.as_ref(),
                        &request,
                        &state.members.into_values().collect::<Vec<_>>(),
                    )
                    .map_err(|_| unsupported())?;
                    encode_service_message(&challenge)?
                }
                ("cluster-consensus-rpc/v1", "vote") => {
                    use bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService;
                    let request: bmux_cluster_plugin_api::cluster_consensus_rpc::client::VoteRequest = decode_service_message(&invocation.payload)?;
                    let result = tokio::task::block_in_place(|| {
                        runtime.block_on(provider.vote(request.request))
                    });
                    encode_service_message(&result)?
                }
                ("cluster-consensus-rpc/v2", "append") => {
                    use bmux_cluster_plugin_api::cluster_consensus_rpc_v2::ClusterConsensusRpcService;
                    let request: bmux_cluster_plugin_api::cluster_consensus_rpc_v2::client::AppendRequest = decode_service_message(&invocation.payload)?;
                    let (_, handle) = registry
                        .iter()
                        .find(|(_, handle)| {
                            handle.interface_id().as_str() == "cluster-consensus-rpc/v2"
                        })
                        .ok_or_else(unsupported)?;
                    let service = handle
                        .provider_as_trait::<dyn ClusterConsensusRpcService + Send + Sync>()
                        .map_err(|_| unsupported())?;
                    let probe = format!(
                        "v2 {} -> {} term={} prev={:?} entries={} commit={:?}",
                        self.endpoint,
                        remote.endpoint,
                        request.request.term,
                        request.request.previous,
                        request.request.entries.len(),
                        request.request.leader_commit
                    );
                    let result = tokio::task::block_in_place(|| {
                        runtime.block_on(service.append(request.request))
                    });
                    self.network.record(format!("{probe} result={result:?}"));
                    encode_service_message(&result)?
                }
                ("cluster-consensus-rpc/v1", "append") => {
                    use bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService;
                    let request: bmux_cluster_plugin_api::cluster_consensus_rpc::client::AppendRequest = decode_service_message(&invocation.payload)?;
                    let probe = format!(
                        "{} -> {} term={} prev={:?} entries={} commit={:?}",
                        self.endpoint,
                        remote.endpoint,
                        request.request.term,
                        request.request.previous,
                        request.request.entries.len(),
                        request.request.leader_commit
                    );
                    let result = tokio::task::block_in_place(|| {
                        runtime.block_on(provider.append(request.request))
                    });
                    self.network.record(format!("{probe} result={result:?}"));
                    encode_service_message(&result)?
                }
                _ => return Err(unsupported()),
            };
            let response = encode_service_message(&Result::<Vec<u8>, ConnectionError>::Ok(result))?;
            Ok(bmux_plugin_sdk::service_contract::encode_response(
                &response,
            ))
        }
    }

    struct Registration(Arc<TestEndpointCaller>);
    impl Drop for Registration {
        fn drop(&mut self) {
            self.0.executor.lock().unwrap().take();
        }
    }

    // This fixture constructs signed credentials, canonical membership and the
    // same initial log on every fresh store as one explicit provisioning step.
    #[allow(clippy::too_many_lines)]
    fn fixed_test_cluster(
        root: &std::path::Path,
        count: usize,
    ) -> (
        Vec<Arc<TestEndpointCaller>>,
        Vec<PathBuf>,
        Vec<NodeId>,
        String,
    ) {
        use crate::consensus_model::{AppendRequest, Entry, EntryPayload, LogIndex, Term};
        use bmux_cluster_plugin_api::cluster_types::{
            ClusterConsensusRole, ClusterNodeCapabilities,
        };
        let network = Arc::new(TestEndpointNetwork::default());
        let callers = (0..count)
            .map(|index| {
                Arc::new(TestEndpointCaller {
                    endpoint: format!("owned-node-{index}"),
                    network: network.clone(),
                    storage: std::sync::Mutex::new(BTreeMap::new()),
                    executor: std::sync::Mutex::new(None),
                    scope: crate::durable_storage::TestScope::default(),
                })
            })
            .collect::<Vec<_>>();
        let identities = callers
            .iter()
            .map(|caller| crate::load_or_create_node_identity(caller.as_ref()).unwrap())
            .collect::<Vec<_>>();
        let cluster = crate::membership::ClusterId::generate();
        let protocol = crate::membership::current_protocol_offer();
        let members = identities
            .iter()
            .enumerate()
            .map(|(index, identity)| {
                let mut member = crate::membership::issue_membership_credential(
                    &identities[0],
                    cluster,
                    identity.node_id().to_string(),
                    identity.public_key().to_string(),
                    ClusterNodeCapabilities {
                        ingress: true,
                        worker: true,
                        consensus_role: ClusterConsensusRole::Voter,
                    },
                    bmux_cluster_plugin_api::cluster_types::ClusterNegotiatedProtocol {
                        wire_epoch: protocol.wire_epoch,
                        local_plugin_version: protocol.plugin_version.clone(),
                        remote_plugin_version: protocol.plugin_version.clone(),
                        peer_revision: 1,
                        schema_version: 1,
                        features: vec![
                            bmux_cluster_plugin_api::cluster_consensus_rpc_v2::INTERFACE_ID
                                .to_string(),
                            bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID
                                .to_string(),
                            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID
                                .to_string(),
                        ],
                    },
                    crate::now_unix_ms(),
                )
                .unwrap();
                member.endpoint = Some(callers[index].endpoint.clone());
                member
            })
            .collect::<Vec<_>>();
        let ids = identities
            .iter()
            .map(|identity| *identity.node_id())
            .collect::<Vec<_>>();
        let membership = Membership::new(
            vec![ids.clone()],
            members
                .iter()
                .map(|member| {
                    (
                        member.node_id.parse().unwrap(),
                        member.endpoint.clone().unwrap(),
                    )
                })
                .collect(),
        )
        .unwrap();
        let mut paths = Vec::new();
        for (index, caller) in callers.iter().enumerate() {
            caller.storage.lock().unwrap().insert(
                crate::membership::CLUSTER_ID_STORAGE_KEY.into(),
                serde_json::to_vec(
                    &serde_json::json!({"version": 1, "cluster_id": cluster.to_string()}),
                )
                .unwrap(),
            );
            let state: crate::membership::MembershipState = serde_json::from_value(serde_json::json!({
                "version": 1, "cluster_id": cluster.to_string(), "issuer_endpoint": null,
                "members": members.iter().map(|member| (member.node_id.clone(), member.clone())).collect::<BTreeMap<_, _>>(),
                "enrollment_tokens": {}, "accepted_rotations": {}
            })).unwrap();
            crate::membership::store_membership_state(caller.as_ref(), &state).unwrap();
            network
                .nodes
                .lock()
                .unwrap()
                .insert(caller.endpoint.clone(), caller.clone());
            let path = root.join(format!("node-{index}.redb"));
            let mut store = OwnedVoteStore::create(&path, &cluster.to_string()).unwrap();
            let identity = |index| LogIdentity {
                term: Term(0),
                leader: ids[0],
                index: LogIndex(index),
            };
            let mut entries = vec![Entry {
                identity: identity(0),
                payload: EntryPayload::Membership(membership.clone()),
            }];
            for (index, member) in members.iter().enumerate() {
                let mut command = command(u128::try_from(index).unwrap() + 10);
                command.issued_at_unix_ms = member.credential_issued_at_unix_ms;
                command.request = ControlCommandRequest::UpsertMember {
                    member: member.clone(),
                };
                entries.push(Entry {
                    identity: identity(u64::try_from(index).unwrap() + 1),
                    payload: EntryPayload::Application(
                        crate::control_codec::encode_control_command(&command),
                    ),
                });
            }
            assert!(
                store
                    .append(
                        &AppendRequest {
                            term: Term(0),
                            leader: ids[0],
                            previous: None,
                            leader_commit: Some(identity(u64::try_from(count).unwrap())),
                            entries
                        },
                        (64, COMMAND_BYTES)
                    )
                    .unwrap()
                    .outcome
                    .is_ok()
            );
            store.close().unwrap();
            paths.push(path);
        }
        (callers, paths, ids, cluster.to_string())
    }

    async fn start_fixed(
        caller: Arc<TestEndpointCaller>,
        path: PathBuf,
        cluster: String,
        local: NodeId,
        incarnation: u128,
    ) -> OwnedRuntime {
        let registration_caller = caller.clone();
        OwnedRuntime::recover_fixed(path, cluster, local, incarnation, caller, move |executor| {
            *registration_caller.executor.lock().unwrap() = Some(executor);
            Ok(Registration(registration_caller))
        })
        .await
        .unwrap()
    }

    async fn start_automatic(
        caller: Arc<TestEndpointCaller>,
        path: PathBuf,
        cluster: String,
        local: NodeId,
        incarnation: u128,
    ) -> OwnedRuntime {
        retry_capacity(|| {
            let registration_caller = caller.clone();
            OwnedRuntime::recover_automatic(
                path.clone(),
                cluster.clone(),
                local,
                incarnation,
                caller.clone(),
                move |executor| {
                    *registration_caller.executor.lock().unwrap() = Some(executor);
                    Ok(Registration(registration_caller))
                },
            )
        })
        .await
        .unwrap()
    }

    async fn automatic_leader(
        nodes: &[OwnedRuntime],
        callers: &[Arc<TestEndpointCaller>],
        eligible: &[usize],
    ) -> usize {
        let result = tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                for &index in eligible {
                    if nodes[index].observe().await.is_ok_and(|observation| {
                        observation.local_leader
                            && observation.applied == observation.committed
                            && observation.committed.is_some_and(|position| {
                                position.term == observation.hard_state.term
                            })
                    }) {
                        return index;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        match result {
            Ok(index) => index,
            Err(error) => panic!(
                "automatic election failed: {error:?}; {}",
                topology_diagnostics(nodes, callers).await
            ),
        }
    }

    async fn automatic_proposal(
        nodes: &[OwnedRuntime],
        callers: &[Arc<TestEndpointCaller>],
        leader: usize,
        command_id: u128,
    ) -> ControlResponse {
        match retry_capacity(|| {
            nodes[leader].submit("principal:test".into(), command(command_id), |_, _| Ok(()))
        })
        .await
        {
            Ok(response) => response,
            Err(error) => panic!(
                "automatic proposal {command_id} on node {leader} failed: {error:?}; {}",
                topology_diagnostics(nodes, callers).await
            ),
        }
    }

    async fn automatic_observation(
        nodes: &[OwnedRuntime],
        callers: &[Arc<TestEndpointCaller>],
        index: usize,
    ) -> OwnedObservation {
        match retry_capacity(|| nodes[index].observe()).await {
            Ok(observation) => observation,
            Err(error) => panic!(
                "automatic observation node {index} failed: {error:?}; {}",
                topology_diagnostics(nodes, callers).await
            ),
        }
    }

    async fn automatic_generated_transport_scenario(count: usize) {
        let root = tempfile::tempdir().unwrap();
        let (callers, paths, ids, cluster) = fixed_test_cluster(root.path(), count);
        let mut nodes = Vec::new();
        for index in 0..count {
            nodes.push(
                start_automatic(
                    callers[index].clone(),
                    paths[index].clone(),
                    cluster.clone(),
                    ids[index],
                    1,
                )
                .await,
            );
        }
        let eligible = (0..count).collect::<Vec<_>>();
        let leader = automatic_leader(&nodes, &callers, &eligible).await;
        let term = automatic_observation(&nodes, &callers, leader)
            .await
            .hard_state
            .term;
        let first = automatic_proposal(&nodes, &callers, leader, 1).await;
        // More than the maximum randomized election timeout, with no explicit
        // campaign or client traffic needed to retain the elected term.
        tokio::time::sleep(Duration::from_secs(7)).await;
        for node in &nodes {
            assert_eq!(
                retry_capacity(|| node.observe())
                    .await
                    .unwrap()
                    .hard_state
                    .term,
                term,
                "stable term changed: {}",
                topology_diagnostics(&nodes, &callers).await
            );
        }
        assert_eq!(
            automatic_leader(&nodes, &callers, &eligible).await,
            leader,
            "stable leader changed: {}",
            topology_diagnostics(&nodes, &callers).await
        );
        let authorized = checked_quorum_read(leader, &nodes, &callers).await;
        assert!(authorized.state.revision >= first.control_revision);

        // Complete joined shutdown, then require an automatic replacement.
        // The old node remains registered in the test routing map but its
        // authority registration is gone: reachability cannot confer a vote.
        nodes[leader].shutdown().await.unwrap();
        callers[0]
            .network
            .delays
            .lock()
            .unwrap()
            .insert(callers[leader].endpoint.clone(), Duration::from_secs(8));
        let surviving = eligible
            .into_iter()
            .filter(|index| *index != leader)
            .collect::<Vec<_>>();
        let replacement = automatic_leader(&nodes, &callers, &surviving).await;
        assert!(
            automatic_observation(&nodes, &callers, replacement)
                .await
                .hard_state
                .term
                > term,
            "replacement term did not advance: {}",
            topology_diagnostics(&nodes, &callers).await
        );
        automatic_proposal(&nodes, &callers, replacement, 2).await;
        let replacement_term = automatic_observation(&nodes, &callers, replacement)
            .await
            .hard_state
            .term;
        // The unavailable peer occupies a transport turn longer than the
        // election timeout. Healthy-majority heartbeats and local application
        // must continue independently of that slot.
        tokio::time::sleep(Duration::from_secs(7)).await;
        for &index in &surviving {
            assert_eq!(
                automatic_observation(&nodes, &callers, index)
                    .await
                    .hard_state
                    .term,
                replacement_term,
                "replacement stable term changed: {}",
                topology_diagnostics(&nodes, &callers).await
            );
        }
        callers[0].network.delays.lock().unwrap().clear();
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
        callers[0].network.nodes.lock().unwrap().clear();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn automatic_three_voter_generated_transport_heartbeat_and_failover() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        automatic_generated_transport_scenario(3).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn automatic_five_voter_generated_transport_heartbeat_and_failover() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        automatic_generated_transport_scenario(5).await;
    }

    /// Caller-side backpressure only. Capacity proves no proposal/read round
    /// was admitted; never retry an uncertain mutation or quorum failure.
    async fn retry_capacity<T, F, Fut>(mut attempt: F) -> Result<T, RuntimeError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, RuntimeError>>,
    {
        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        loop {
            // Each attempt owns its runtime's bounded completion deadline.
            // An outer timeout must not cancel admitted waiting and relabel an
            // uncertain mutation as definitive pre-admission Capacity.
            match attempt().await {
                Err(RuntimeError::Capacity) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep_until(
                        (tokio::time::Instant::now() + Duration::from_millis(20)).min(deadline),
                    )
                    .await;
                }
                result => return result,
            }
        }
    }

    async fn topology_diagnostics(
        nodes: &[OwnedRuntime],
        callers: &[Arc<TestEndpointCaller>],
    ) -> String {
        let mut report = Vec::new();
        for (index, caller) in callers.iter().enumerate() {
            let (send, receive) = oneshot::channel();
            let scheduler = match nodes[index].admit(Request::Diagnostics(send)) {
                Ok(()) => format!(
                    "{:?}",
                    tokio::time::timeout(Duration::from_secs(1), receive).await
                ),
                Err(error) => format!("diagnostic admission: {error:?}"),
            };
            let executor = caller.executor.lock().unwrap().clone();
            if let Some(executor) = executor {
                let local = *crate::load_or_create_node_identity(caller.as_ref())
                    .unwrap()
                    .node_id();
                let mut probes = Vec::new();
                for peer in callers {
                    let peer_id = *crate::load_or_create_node_identity(peer.as_ref())
                        .unwrap()
                        .node_id();
                    probes.push((
                        peer.endpoint.clone(),
                        executor.replication_next(peer_id).await,
                    ));
                }
                report.push(format!(
                    "{} scheduler={} durable={:?} probes={probes:?}",
                    caller.endpoint,
                    scheduler,
                    executor.observe(local).await
                ));
            } else {
                report.push(format!(
                    "{} scheduler={} registration absent",
                    caller.endpoint, scheduler
                ));
            }
        }
        format!(
            "nodes={report:?}; blocked={:?}; last32_rpc={:?}",
            callers[0].network.blocked.lock().unwrap(),
            callers[0].network.trace.lock().unwrap()
        )
    }

    async fn checked_quorum_read(
        index: usize,
        nodes: &[OwnedRuntime],
        callers: &[Arc<TestEndpointCaller>],
    ) -> AuthorizedRead {
        match retry_capacity(|| nodes[index].read_linearizable()).await {
            Ok(read) => read,
            Err(error) => panic!(
                "quorum read failed: {error:?}; {}",
                topology_diagnostics(nodes, callers).await
            ),
        }
    }

    async fn wait_owned_leader(
        index: usize,
        nodes: &[OwnedRuntime],
        callers: &[Arc<TestEndpointCaller>],
    ) {
        let runtime = &nodes[index];
        let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                if runtime.observe().await.is_ok_and(|observation| {
                    observation.local_leader
                        && observation
                            .committed
                            .is_some_and(|position| position.term == observation.hard_state.term)
                        && observation.applied == observation.committed
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "leader wait failed for {index}: {}",
            topology_diagnostics(nodes, callers).await
        );
    }

    async fn heal_and_check_catchup(nodes: &[OwnedRuntime], callers: &[Arc<TestEndpointCaller>]) {
        callers[0].network.blocked.lock().unwrap().clear();
        let mut last = None;
        let caught_up = tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                let follower = retry_capacity(|| nodes[0].observe()).await;
                let leader = retry_capacity(|| nodes[1].observe()).await;
                let ready = matches!((&follower, &leader), (Ok(follower), Ok(leader)) if !follower.local_leader && follower.applied == leader.applied);
                last = Some((follower, leader));
                if ready { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await;
        if caught_up.is_err() {
            let mut positions = Vec::new();
            for caller in callers {
                let executor = caller.executor.lock().unwrap().clone();
                if let Some(executor) = executor {
                    let local = *crate::load_or_create_node_identity(caller.as_ref())
                        .unwrap()
                        .node_id();
                    let mut next = Vec::new();
                    for peer in callers {
                        let peer_id = *crate::load_or_create_node_identity(peer.as_ref())
                            .unwrap()
                            .node_id();
                        next.push((
                            peer.endpoint.clone(),
                            executor.replication_next(peer_id).await,
                        ));
                    }
                    positions.push((caller.endpoint.clone(), executor.observe(local).await, next));
                }
            }
            let trace = callers[0].network.trace.lock().unwrap().clone();
            let mut shutdown = Vec::new();
            for node in nodes {
                shutdown.push(node.shutdown().await);
            }
            panic!(
                "heal catch-up failed: last={last:?}; durable={positions:?}; last32_rpc={trace:?}; shutdown={shutdown:?}"
            );
        }
    }

    async fn wait_failover_candidate_prefix(
        nodes: &[OwnedRuntime],
        callers: &[Arc<TestEndpointCaller>],
        watermark: LogIdentity,
    ) {
        let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
            loop {
                if retry_capacity(|| nodes[1].observe())
                    .await
                    .is_ok_and(|state| {
                        state.applied == Some(watermark) && state.last_log == Some(watermark)
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "candidate did not receive committed prefix: {}",
            topology_diagnostics(nodes, callers).await
        );
    }

    fn isolate_first_peer(callers: &[Arc<TestEndpointCaller>]) {
        let mut blocked = callers[0].network.blocked.lock().unwrap();
        for peer in callers.iter().skip(1) {
            blocked.insert((callers[0].endpoint.clone(), peer.endpoint.clone()));
            blocked.insert((peer.endpoint.clone(), callers[0].endpoint.clone()));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn actual_three_voter_transport_partition_and_restart() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let (callers, paths, ids, cluster) = fixed_test_cluster(root.path(), 3);
        let mut nodes = Vec::new();
        for index in 0..3 {
            nodes.push(
                start_fixed(
                    callers[index].clone(),
                    paths[index].clone(),
                    cluster.clone(),
                    ids[index],
                    1,
                )
                .await,
            );
        }
        nodes[0].campaign().await.unwrap();
        wait_owned_leader(0, &nodes, &callers).await;
        let first =
            retry_capacity(|| nodes[0].submit("principal:test".into(), command(1), |_, _| Ok(())))
                .await
                .unwrap();
        let authorized = checked_quorum_read(0, &nodes, &callers).await;
        assert!(authorized.state.revision >= first.control_revision);
        assert_eq!(
            authorized.watermark.term,
            retry_capacity(|| nodes[0].observe())
                .await
                .unwrap()
                .hard_state
                .term
        );
        // A quorum read need not include the node we intend to campaign next.
        // Establish that candidate's actual durable prefix, not inferred quorum
        // participation; otherwise a fresher surviving voter must deny its vote.
        wait_failover_candidate_prefix(&nodes, &callers, authorized.watermark).await;
        // Isolate both directions. A reachable minority cannot manufacture a
        // commit even while retaining its old local leader state.
        isolate_first_peer(&callers);
        let committed = retry_capacity(|| nodes[0].observe())
            .await
            .unwrap()
            .committed;
        let minority = async {
            let (mutation, read) = tokio::join!(
                retry_capacity(
                    || nodes[0].submit("principal:test".into(), command(2), |_, _| Ok(()))
                ),
                nodes[0].read_linearizable(),
            );
            assert!(matches!(
                read,
                Err(RuntimeError::QuorumUnavailable | RuntimeError::Capacity)
            ));
            mutation
        };
        let majority = async {
            retry_capacity(|| nodes[1].campaign()).await.unwrap();
            wait_owned_leader(1, &nodes, &callers).await;
            retry_capacity(|| nodes[1].submit("principal:test".into(), command(3), |_, _| Ok(())))
                .await
                .unwrap()
        };
        let (minority, _) = tokio::join!(minority, majority);
        assert_eq!(
            minority,
            Err(RuntimeError::Indeterminate),
            "minority proposal outcome: {}",
            topology_diagnostics(&nodes, &callers).await
        );
        assert_eq!(
            retry_capacity(|| nodes[0].observe())
                .await
                .unwrap()
                .committed,
            committed
        );
        heal_and_check_catchup(&nodes, &callers).await;
        nodes[0].shutdown().await.unwrap();
        nodes[0] = start_fixed(callers[0].clone(), paths[0].clone(), cluster, ids[0], 2).await;
        assert_eq!(
            retry_capacity(|| nodes[1].submit("principal:test".into(), command(1), |_, _| Ok(())))
                .await
                .unwrap(),
            first
        );
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
        callers[0].network.nodes.lock().unwrap().clear();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn actual_five_voter_generated_transport_commits_with_two_unreachable() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let (callers, paths, ids, cluster) = fixed_test_cluster(root.path(), 5);
        let mut nodes = Vec::new();
        for index in 0..5 {
            nodes.push(
                start_fixed(
                    callers[index].clone(),
                    paths[index].clone(),
                    cluster.clone(),
                    ids[index],
                    1,
                )
                .await,
            );
        }
        for peer in 3..5 {
            for source in 0..3 {
                callers[0].network.blocked.lock().unwrap().insert((
                    callers[source].endpoint.clone(),
                    callers[peer].endpoint.clone(),
                ));
                callers[0].network.blocked.lock().unwrap().insert((
                    callers[peer].endpoint.clone(),
                    callers[source].endpoint.clone(),
                ));
            }
        }
        nodes[0].campaign().await.unwrap();
        wait_owned_leader(0, &nodes, &callers).await;
        let response = nodes[0]
            .submit("principal:test".into(), command(1), |_, _| Ok(()))
            .await
            .unwrap();
        assert_eq!(response.command_id, command(1).command_id);
        for node in &nodes {
            node.shutdown().await.unwrap();
        }
        callers[0].network.nodes.lock().unwrap().clear();
    }

    struct FairnessTransport {
        calls: Arc<std::sync::Mutex<Vec<NodeId>>>,
        slow: NodeId,
    }

    impl OwnedTransport for FairnessTransport {
        fn vote(
            &self,
            executor: Arc<VoteExecutor>,
            _request: RequestVote,
            peer: NodeId,
        ) -> PeerWork {
            self.replicate(executor, peer)
        }
        fn confirm(
            &self,
            executor: Arc<VoteExecutor>,
            _context: ReadContext,
            peer: NodeId,
        ) -> PeerWork {
            self.replicate(executor, peer)
        }
        fn replicate(&self, _executor: Arc<VoteExecutor>, peer: NodeId) -> PeerWork {
            let calls = self.calls.clone();
            let slow = self.slow;
            Box::pin(async move {
                calls.lock().unwrap().push(peer);
                if peer == slow {
                    tokio::task::yield_now().await;
                    Err(RpcError::CapacityUnavailable)
                } else {
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn round_robin_serializes_shared_admission_and_retries_busy_read_peer() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let local = NodeId::from(1);
        let peers = [NodeId::from(2), NodeId::from(3)];
        let path = root.path().join("scheduler.redb");
        bootstrap(&path, local);
        let executor = Arc::new(
            VoteExecutor::recover(path, "owned-runtime".into(), 1)
                .await
                .unwrap(),
        );
        initialize(&executor, local).await.unwrap();
        let observation = executor.observe(local).await.unwrap();
        let context = executor.begin_read(local).await.unwrap().unwrap();
        let (response, _receive) = oneshot::channel();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut fixed = FixedPeers {
            automatic: None,
            next_heartbeat: tokio::time::Instant::now(),
            read_turn: false,
            cursor: 0,
            recent: std::collections::VecDeque::new(),
            ticks: 0,
            busy_ticks: 0,
            read: Some(PendingRead {
                context,
                response,
                deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT,
                attempted: BTreeSet::new(),
            }),
            membership: Membership::new(
                vec![vec![local, peers[0], peers[1]]],
                vec![
                    (local, String::new()),
                    (peers[0], String::new()),
                    (peers[1], String::new()),
                ],
            )
            .unwrap(),
            transport: Arc::new(FairnessTransport {
                calls: calls.clone(),
                slow: peers[1],
            }),
            tasks: JoinSet::new(),
            in_flight: BTreeSet::new(),
            campaign: None,
            pending: BTreeMap::new(),
        };
        // This test checks scheduling only: transport results never manufacture
        // votes, matches or read confirmations in the real executor.
        for _ in 0..6 {
            schedule_peers(&executor, local, &mut fixed, &observation);
            assert_eq!(fixed.tasks.len(), 1);
            schedule_peers(&executor, local, &mut fixed, &observation);
            assert_eq!(fixed.tasks.len(), 1, "second peer raced shared admission");
            let (peer, result) = fixed.tasks.join_next().await.unwrap().unwrap();
            fixed.completed(peer, result);
            if peer == peers[1] {
                assert!(!fixed.read.as_ref().unwrap().attempted.contains(&peer));
            }
        }
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 6);
        for pair in calls.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }
        assert_eq!(calls.iter().filter(|peer| **peer == peers[0]).count(), 3);
        assert_eq!(calls.iter().filter(|peer| **peer == peers[1]).count(), 3);
        exercise_automatic_fairness(&executor, local, &mut fixed, &observation, peers, context)
            .await;
        executor.shutdown().await.unwrap();
    }

    async fn exercise_automatic_fairness(
        executor: &Arc<VoteExecutor>,
        local: NodeId,
        fixed: &mut FixedPeers,
        observation: &OwnedObservation,
        peers: [NodeId; 2],
        context: ReadContext,
    ) {
        fixed.automatic = Some(AutomaticElection::seeded(
            tokio::time::Instant::now(),
            [3; 32],
        ));
        let expired = tokio::time::Instant::now() - Duration::from_secs(1);
        fixed.automatic.as_mut().unwrap().deadline = expired;
        let mut stale = observation.clone();
        stale.local_leader = false;
        stale.hard_state.term = crate::consensus_model::Term(stale.hard_state.term.0 + 1);
        automatic_campaign(executor, local, fixed, &stale)
            .await
            .unwrap();
        assert_eq!(
            fixed.automatic.as_ref().unwrap().deadline,
            expired,
            "failed guarded campaign redrew its expired deadline"
        );
        assert!(fixed.campaign.is_none());
        fixed.read = None;
        let (release, held) = oneshot::channel();
        fixed.in_flight.insert(peers[1]);
        let slow = peers[1];
        fixed.tasks.spawn(async move {
            let _ = held.await;
            (slow, Ok(()))
        });
        for _ in 0..4 {
            fixed.next_heartbeat = tokio::time::Instant::now();
            schedule_peers(executor, local, fixed, observation);
            assert_eq!(fixed.tasks.len(), 2, "slow peer blocked healthy admission");
            let (peer, result) = fixed.tasks.join_next().await.unwrap().unwrap();
            assert_eq!(peer, peers[0]);
            fixed.completed(peer, result);
        }
        // Cancelling a waiter must release only waiting state, without needing
        // a storage admission or cancelling a peer task.
        let (send, receive) = oneshot::channel();
        drop(receive);
        fixed.read = Some(PendingRead {
            context,
            response: send,
            deadline: tokio::time::Instant::now() + REQUEST_TIMEOUT,
            attempted: BTreeSet::new(),
        });
        fixed.expire_waiters(tokio::time::Instant::now());
        assert!(fixed.read.is_none());
        assert_eq!(fixed.tasks.len(), 1);
        release.send(()).unwrap();
        drain_peer_tasks(&mut fixed.tasks).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_drain_reports_late_panic_and_still_joins_other_work() {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        let (stop, stopping) = oneshot::channel();
        tasks.spawn(async move {
            assert!(stopping.await.is_err(), "peer task panicked after stop");
            (NodeId::from(1), Ok(()))
        });
        let completion = completed.clone();
        tasks.spawn(async move {
            tokio::task::yield_now().await;
            completion.store(true, std::sync::atomic::Ordering::SeqCst);
            (NodeId::from(2), Ok(()))
        });
        stop.send(()).unwrap();
        assert!(matches!(
            drain_peer_tasks(&mut tasks).await,
            Err(RuntimeError::Authority(_))
        ));
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(tasks.is_empty());
    }

    #[test]
    fn recovery_capacity_classification_does_not_retry_storage_failures() {
        assert_eq!(
            recovery_error(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
            RuntimeError::Capacity
        );
        for kind in [
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Other,
        ] {
            assert!(matches!(
                recovery_error(std::io::Error::from(kind)),
                RuntimeError::Authority(_)
            ));
        }
    }

    #[test]
    fn randomized_election_deadlines_stay_above_heartbeat_budget() {
        let now = tokio::time::Instant::now();
        let mut first = AutomaticElection::seeded(now, [7; 32]);
        let mut replay = AutomaticElection::seeded(now, [7; 32]);
        let initial = first.deadline;
        let mut changed = false;
        for _ in 0..128 {
            let timeout = first.timeout();
            assert_eq!(timeout, replay.timeout());
            changed |= now + timeout != initial;
            assert!(timeout >= Duration::from_millis(ELECTION_MIN_MILLIS));
            assert!(timeout < Duration::from_millis(ELECTION_MIN_MILLIS + ELECTION_JITTER_MILLIS));
            assert!(timeout > HEARTBEAT_INTERVAL * 4);
        }
        assert!(changed, "seeded retries must redraw jitter");
    }

    #[test]
    fn activity_deadline_uses_receive_time_and_does_not_redraw_on_poll() {
        use crate::owned_vote_store::{ElectionActivity, ElectionActivityKind};
        let now = tokio::time::Instant::now();
        let mut election = AutomaticElection::seeded(now, [9; 32]);
        for (sequence, kind) in [
            (1, ElectionActivityKind::GrantedVote),
            (2, ElectionActivityKind::AuthenticatedLeader),
        ] {
            let activity = ElectionActivity {
                sequence,
                term: crate::consensus_model::Term(2),
                peer: NodeId::from(2),
                kind,
                observed_at: (now + Duration::from_secs(sequence)).into_std(),
            };
            assert!(election.observe_activity(activity));
            let deadline = election.deadline;
            assert!(
                deadline
                    >= now
                        + Duration::from_secs(sequence)
                        + Duration::from_millis(ELECTION_MIN_MILLIS)
            );
            assert!(!election.observe_activity(activity));
            assert_eq!(election.deadline, deadline);
        }
    }

    #[tokio::test]
    async fn empty_owned_store_is_not_silently_bootstrapped() {
        let _scenario = RECOVERY_SCENARIO.lock().await;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owned.redb");
        OwnedVoteStore::create(&path, "owned-runtime")
            .unwrap()
            .close()
            .unwrap();
        let result =
            OwnedRuntime::recover(path.clone(), "owned-runtime".into(), NodeId::from(1), 1).await;
        assert!(matches!(result, Err(RuntimeError::UnsupportedMembership)));
        // Failed readiness joins cleanup and releases the exclusive owner.
        OwnedVoteStore::open(&path, "owned-runtime")
            .unwrap()
            .close()
            .unwrap();
    }
}
