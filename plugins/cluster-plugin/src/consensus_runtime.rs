//! Control-plane consensus node: the BMUX-owned Raft engine (ADR-0015) behind
//! the product API the cluster services use.
//!
//! A [`ConsensusNode`] runs one [`Driver`] over the node's owned durable store
//! ([`OwnedStorage`]) and an injectable transport. Production nodes use the
//! authenticated generated consensus contracts ([`WireTransport`] outbound,
//! [`WireService`] inbound); deterministic tests substitute an in-process
//! transport over the same engine, driver and store.
//!
//! Command encoding, authorization preconditions and response decoding are the
//! product's: proposals carry the canonical control-command bytes and their
//! responses are the canonical control responses produced by application.

use crate::consensus_driver::{
    ClientError, Driver, DriverConfig, Jitter, SnapshotObject, Status, Storage, Transport,
};
use crate::consensus_engine::{Config, Engine, Rejection, StopReason};
use crate::consensus_model::{LogIdentity, Membership, StoredMembership};
use crate::consensus_wire::{MemberView, SnapshotStaging, WireService, WireTransport};
use crate::control_state::ControlState;
use crate::membership::NodeId;
use crate::owned_vote_store::{OwnedStorage, OwnedVoteStore, PreflightAdmission};
use bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService as RpcV1;
use bmux_cluster_plugin_api::cluster_consensus_rpc_v2::ClusterConsensusRpcService as RpcV2;
use bmux_cluster_plugin_api::cluster_consensus_snapshot::ClusterConsensusSnapshotService as SnapshotV1;
use bmux_cluster_plugin_api::cluster_consensus_transfer::ClusterConsensusTransferService as TransferV1;
use bmux_cluster_plugin_api::cluster_types::{
    ControlReadConsistency, ControlResponse, ControlServiceError, ControlStateView,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

#[cfg(test)]
pub(crate) mod tests;

/// A quorum confirmation that has not completed by now reports quorum loss.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// A proposal that has not applied by now has an unknown outcome.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// One membership step (learner admission or promotion) must finish by now.
const MEMBERSHIP_STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounded wait for in-flight calls before durable ownership is released.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the peer credential view is reconciled with local membership.
const MEMBER_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// The owned store file, under the node's state directory. Its name and
/// directory identify the owned durable format; nothing else is selected.
const OWNED_STORE_DIRECTORY: &str = "owned-consensus";
const OWNED_STORE_FILE: &str = "authority.bmown";
/// Disposable inbound snapshot staging, beside the owned store it feeds.
const SNAPSHOT_STAGING_DIRECTORY: &str = "snapshot-staging";
/// Staging quotas (ADR-0015: finite, local resource configuration). One peer
/// may stage one complete snapshot of the largest size the owned store
/// publishes; the node as a whole admits two, so a deposed leader's abandoned
/// transfer cannot block the current leader's until it expires.
const SNAPSHOT_STAGING_PER_PEER_BYTES: u64 =
    crate::consensus_storage::MAX_SNAPSHOT_FILE_BYTES as u64;
const SNAPSHOT_STAGING_TOTAL_BYTES: u64 = 2 * SNAPSHOT_STAGING_PER_PEER_BYTES;
const _: () = assert!(
    SNAPSHOT_STAGING_PER_PEER_BYTES > 0
        && SNAPSHOT_STAGING_TOTAL_BYTES >= SNAPSHOT_STAGING_PER_PEER_BYTES
);
/// Where the retired `OpenRaft` runtime kept its stores. Its presence means
/// this node holds legacy authority the owned runtime will not reinterpret.
const LEGACY_STORE_DIRECTORY: &str = "consensus";

#[derive(Debug)]
pub enum ConsensusRuntimeError {
    Storage(String),
    Configuration(String),
    Fatal(String),
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

impl From<io::Error> for ConsensusRuntimeError {
    fn from(error: io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotLeader {
    pub leader_id: Option<NodeId>,
    pub leader_endpoint: Option<String>,
}

impl std::fmt::Display for NotLeader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.leader_id, &self.leader_endpoint) {
            (Some(id), Some(endpoint)) => {
                write!(formatter, "not leader; forward to {id} at {endpoint}")
            }
            (Some(id), None) => write!(formatter, "not leader; forward to {id}"),
            (None, _) => formatter.write_str("not leader; current leader is unknown"),
        }
    }
}

#[derive(Debug)]
pub enum ConsensusWriteError {
    NotLeader(NotLeader),
    QuorumUnavailable(String),
    Fatal(String),
    Membership(String),
}

impl std::fmt::Display for ConsensusWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLeader(forward) => forward.fmt(formatter),
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
    Fatal(String),
    Storage(String),
}

impl std::fmt::Display for ConsensusReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLeader(forward) => forward.fmt(formatter),
            Self::QuorumUnavailable(error) => {
                write!(formatter, "linearizable read unavailable: {error}")
            }
            Self::Fatal(error) => write!(formatter, "consensus runtime failed: {error}"),
            Self::Storage(error) => write!(formatter, "consensus storage failed: {error}"),
        }
    }
}

impl std::error::Error for ConsensusReadError {}

impl From<ConsensusReadError> for ConsensusWriteError {
    fn from(error: ConsensusReadError) -> Self {
        match error {
            ConsensusReadError::NotLeader(not_leader) => Self::NotLeader(not_leader),
            ConsensusReadError::QuorumUnavailable(reason) => Self::QuorumUnavailable(reason),
            ConsensusReadError::Fatal(reason) | ConsensusReadError::Storage(reason) => {
                Self::Fatal(reason)
            }
        }
    }
}

/// The applied control state and the committed membership it was applied under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedView {
    pub state: ControlState,
    pub membership: Option<StoredMembership>,
}

/// How a node with no store yet initializes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provisioning {
    /// Found a single-voter cluster at this endpoint.
    Founder { endpoint: String },
    /// Start empty and wait for a leader to admit this node.
    Joiner,
}

/// The node's owned store path for `cluster_id`.
#[must_use]
pub fn owned_store_path(state_dir: &Path, cluster_id: &str) -> PathBuf {
    plugin_state_dir(state_dir)
        .join(OWNED_STORE_DIRECTORY)
        .join(cluster_id)
        .join(OWNED_STORE_FILE)
}

/// The node's private snapshot staging directory for `cluster_id`.
#[must_use]
pub fn snapshot_staging_path(state_dir: &Path, cluster_id: &str) -> PathBuf {
    plugin_state_dir(state_dir)
        .join(OWNED_STORE_DIRECTORY)
        .join(cluster_id)
        .join(SNAPSHOT_STAGING_DIRECTORY)
}

fn plugin_state_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("plugins").join("bmux.cluster")
}

/// Refuse to start beside retained `OpenRaft` authority (ADR-0015 clean break):
/// it is neither imported nor modified, and no other directory is chosen.
///
/// # Errors
/// Reports the legacy store location.
pub fn reject_legacy_store(state_dir: &Path) -> Result<(), ConsensusRuntimeError> {
    let legacy = plugin_state_dir(state_dir).join(LEGACY_STORE_DIRECTORY);
    match std::fs::symlink_metadata(&legacy) {
        Ok(_) => Err(ConsensusRuntimeError::Configuration(format!(
            "legacy OpenRaft consensus state exists at {}; the owned consensus runtime is a \
             fresh-install clean break and never imports, modifies or bypasses it. Retire \
             that node's legacy cluster state explicitly (move the directory aside) and \
             re-enroll it into a new cluster",
            legacy.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConsensusRuntimeError::Storage(format!(
            "cannot verify the absence of legacy consensus state at {}: {error}",
            legacy.display()
        ))),
    }
}

/// Open the owned store, or initialize it exactly once. An existing path is
/// always opened and validated (format, cluster binding, recovery) — never
/// replaced; initialization publishes without replacing, so concurrent
/// initializers cannot both win.
fn open_or_initialize(
    path: &Path,
    cluster_id: &str,
    node_id: NodeId,
    provisioning: &Provisioning,
    admission: &PreflightAdmission,
) -> io::Result<OwnedVoteStore> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return OwnedVoteStore::open_with(path, cluster_id, admission),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("owned store path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    if let Some(grandparent) = parent.parent() {
        switchy_fs::sync::sync_directory(grandparent)?;
    }
    match provisioning {
        Provisioning::Founder { endpoint } => {
            let membership =
                Membership::new(vec![vec![node_id]], vec![(node_id, endpoint.clone())]).map_err(
                    |error| io::Error::other(format!("invalid founding membership: {error:?}")),
                )?;
            OwnedVoteStore::bootstrap(path, cluster_id, node_id, membership)
        }
        Provisioning::Joiner => OwnedVoteStore::create(path, cluster_id),
    }
}

/// Create (owner-only) or reuse the staging directory and open it. Staging
/// requires a trusted absolute path without symlinked components, so the
/// configured state directory is resolved once here; the staging layer then
/// re-walks it without following links and checks every ancestor's ownership.
/// Leftovers from a previous run are discarded: staging is disposable.
fn open_staging(path: &Path) -> io::Result<Arc<SnapshotStaging>> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                switchy_fs::sync::sync_directory(parent)?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let resolved = std::fs::canonicalize(path)?;
    SnapshotStaging::open(
        &resolved,
        SNAPSHOT_STAGING_PER_PEER_BYTES,
        SNAPSHOT_STAGING_TOTAL_BYTES,
    )
}

/// Cached applied view, refreshed after every durable application.
struct AppliedCache {
    view: std::sync::RwLock<AppliedView>,
    revision: tokio::sync::watch::Sender<u64>,
    changed: tokio::sync::Notify,
}

impl AppliedCache {
    fn new(view: AppliedView) -> Self {
        let (revision, _) = tokio::sync::watch::channel(view.state.revision);
        Self {
            view: std::sync::RwLock::new(view),
            revision,
            changed: tokio::sync::Notify::new(),
        }
    }

    fn get(&self) -> AppliedView {
        self.view
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn publish(&self, view: AppliedView) {
        let revision = view.state.revision;
        *self.view.write().unwrap_or_else(PoisonError::into_inner) = view;
        self.revision.send_if_modified(|current| {
            let advanced = *current != revision;
            *current = revision;
            advanced
        });
        self.changed.notify_waiters();
    }
}

/// The owned store as the driver's authority, publishing each applied view.
struct RuntimeStorage {
    inner: OwnedStorage,
    applied: Arc<AppliedCache>,
}

impl RuntimeStorage {
    fn refresh(&mut self) -> io::Result<AppliedView> {
        let (state, membership) = self.inner.applied()?;
        let view = AppliedView { state, membership };
        self.applied.publish(view.clone());
        Ok(view)
    }
}

impl Storage for RuntimeStorage {
    type View = AppliedView;

    fn write(&mut self, write: &crate::consensus_engine::Write) -> io::Result<()> {
        self.inner.write(write)
    }

    fn install_snapshot(
        &mut self,
        from: NodeId,
        last_included: LogIdentity,
        membership: &StoredMembership,
    ) -> io::Result<()> {
        self.inner
            .install_snapshot(from, last_included, membership)?;
        self.refresh().map(|_| ())
    }

    fn apply(&mut self, through: LogIdentity) -> io::Result<Vec<(LogIdentity, Vec<u8>)>> {
        let responses = self.inner.apply(through)?;
        self.refresh()?;
        Ok(responses)
    }

    fn compact(&mut self) -> io::Result<Option<(LogIdentity, StoredMembership)>> {
        self.inner.compact()
    }

    fn snapshot(&mut self) -> io::Result<Option<SnapshotObject>> {
        self.inner.snapshot()
    }

    fn read(&mut self) -> io::Result<AppliedView> {
        self.refresh()
    }

    fn close(self) -> io::Result<()> {
        self.inner.close()
    }
}

/// Election deadlines drawn from a generator seeded per node instance, so
/// nodes never share deadlines.
fn seeded_jitter() -> Jitter {
    let (high, low) = uuid::Uuid::new_v4().as_u64_pair();
    let mut state = (high ^ low) | 1;
    Box::new(move |min, max| {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let draw = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let span = u64::try_from(max.saturating_sub(min).as_millis()).unwrap_or(u64::MAX);
        min + Duration::from_millis(draw % span.saturating_add(1))
    })
}

/// Engine bounds for authenticated endpoint peers: each RPC carries a fresh
/// peer proof and durable work, so leave headroom before suspecting a leader,
/// and replicate one batch per peer at a time.
fn endpoint_config() -> Config {
    Config {
        election_timeout: (Duration::from_millis(1_000), Duration::from_millis(2_000)),
        heartbeat_interval: Duration::from_millis(150),
        max_inflight: 1,
        ..Config::default()
    }
}

/// At most one outstanding call per peer. Every endpoint RPC holds shared host
/// callback capacity for its whole round trip (peer proof, dispatch, reply),
/// so unbounded fan-out under a slow peer would starve the host. A message to
/// a busy peer is dropped as a lossy network would; the engine retries.
struct PeerBoundedTransport<T> {
    inner: Arc<T>,
    busy: std::sync::Mutex<BTreeMap<NodeId, Arc<tokio::sync::Semaphore>>>,
}

impl<T: Transport> PeerBoundedTransport<T> {
    const fn new(inner: Arc<T>) -> Self {
        Self {
            inner,
            busy: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    fn admit(&self, to: NodeId) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(to)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
            .clone()
            .try_acquire_owned()
            .ok()
    }
}

impl<T: Transport> Transport for PeerBoundedTransport<T> {
    fn call(
        &self,
        to: NodeId,
        message: crate::consensus_engine::Message,
    ) -> crate::consensus_driver::Reply {
        let Some(permit) = self.admit(to) else {
            return Box::pin(async { None });
        };
        let call = self.inner.call(to, message);
        Box::pin(async move {
            let reply = call.await;
            drop(permit);
            reply
        })
    }

    fn send_snapshot(
        &self,
        to: NodeId,
        term: crate::consensus_model::Term,
        seq: u64,
        snapshot: SnapshotObject,
    ) -> crate::consensus_driver::Reply {
        let Some(permit) = self.admit(to) else {
            return Box::pin(async { None });
        };
        let call = self.inner.send_snapshot(to, term, seq, snapshot);
        Box::pin(async move {
            let reply = call.await;
            drop(permit);
            reply
        })
    }
}

/// A running node's inbound consensus providers: `cluster-consensus-rpc` v1
/// (vote) and v2 (append), staged `cluster-consensus-transfer` v1 and
/// `cluster-consensus-snapshot` v1 installation.
#[derive(Clone)]
pub struct RpcProviders {
    pub rpc_v1: Arc<dyn RpcV1 + Send + Sync>,
    pub rpc_v2: Arc<dyn RpcV2 + Send + Sync>,
    pub transfer: Arc<dyn TransferV1 + Send + Sync>,
    pub snapshot: Arc<dyn SnapshotV1 + Send + Sync>,
}

struct Inner {
    driver: std::sync::Mutex<Option<Arc<Driver<AppliedView>>>>,
    rpc: std::sync::Mutex<Option<RpcProviders>>,
    /// Inbound snapshot staging, closed during shutdown before the store.
    staging: std::sync::Mutex<Option<Arc<SnapshotStaging>>>,
    status: tokio::sync::watch::Receiver<Status>,
    applied: Arc<AppliedCache>,
    members: MemberView,
    background: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Serializes membership transitions with the writes that depend on them.
    membership_submission: tokio::sync::Mutex<()>,
    node_id: NodeId,
    cluster_id: String,
    /// The last worker lease sequence this node issued.
    lease_sequence: AtomicU64,
}

/// One running owned-consensus node.
#[derive(Clone)]
pub struct ConsensusNode {
    inner: Arc<Inner>,
}

fn validate_admission_term(expected: u64, current: u64) -> Result<(), ConsensusWriteError> {
    if expected != current {
        return Err(ConsensusWriteError::Membership(
            "leadership term changed during admission; retry compatibility checks".into(),
        ));
    }
    Ok(())
}

/// Format a membership epoch from its introducing log identity.
#[must_use]
pub fn membership_epoch(membership: &StoredMembership) -> Option<String> {
    membership
        .introduced_at
        .map(|log| format!("{}:{}:{}", log.term.0, log.leader, log.index.0))
}

impl ConsensusNode {
    /// Starts one node over its owned store with the supplied transport.
    ///
    /// All consensus, application and persistence behavior is identical for
    /// the production endpoint transport and deterministic substitutes.
    ///
    /// # Errors
    /// Returns legacy-store, storage or recovery failures.
    pub async fn start<T: Transport>(
        state_dir: &Path,
        cluster_id: &str,
        node_id: NodeId,
        provisioning: Provisioning,
        admission: &PreflightAdmission,
        transport: impl FnOnce(MemberView) -> Arc<T>,
    ) -> Result<Self, ConsensusRuntimeError> {
        Self::start_with(
            state_dir,
            cluster_id,
            node_id,
            provisioning,
            admission,
            (Config::default(), DriverConfig::default()),
            transport,
        )
        .await
    }

    async fn start_with<T: Transport>(
        state_dir: &Path,
        cluster_id: &str,
        node_id: NodeId,
        provisioning: Provisioning,
        admission: &PreflightAdmission,
        (config, driver_config): (Config, DriverConfig),
        transport: impl FnOnce(MemberView) -> Arc<T>,
    ) -> Result<Self, ConsensusRuntimeError> {
        if cluster_id.is_empty()
            || cluster_id.len() > 128
            || cluster_id.contains(['/', '\\'])
            || cluster_id.starts_with('.')
        {
            return Err(ConsensusRuntimeError::Configuration(
                "invalid cluster identity for the owned store".into(),
            ));
        }
        reject_legacy_store(state_dir)?;
        let path = owned_store_path(state_dir, cluster_id);
        let staging_path = snapshot_staging_path(state_dir, cluster_id);
        let cluster = cluster_id.to_string();
        let admission = admission.clone();
        let (store, recovered, staging) = tokio::task::spawn_blocking(move || -> io::Result<_> {
            let mut store = open_or_initialize(&path, &cluster, node_id, &provisioning, &admission)
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("owned consensus store {}: {error}", path.display()),
                    )
                })?;
            // Staging is opened only by the store's single owner.
            let opened = store.recovered().and_then(|recovered| {
                open_staging(&staging_path)
                    .map(|staging| (recovered, staging))
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("owned snapshot staging {}: {error}", staging_path.display()),
                        )
                    })
            });
            match opened {
                Ok((recovered, staging)) => Ok((store, recovered, staging)),
                Err(error) => {
                    // Release durable ownership before reporting the failure.
                    let _ = store.close();
                    Err(error)
                }
            }
        })
        .await
        .map_err(|error| ConsensusRuntimeError::Fatal(error.to_string()))??;
        let (engine, effects) = Engine::recover(node_id, config, recovered)
            .map_err(|error| ConsensusRuntimeError::Storage(format!("{error:?}")))?;
        let mut storage = OwnedStorage::new(store, staging.reader());
        let (state, membership) = storage.applied()?;
        let applied = Arc::new(AppliedCache::new(AppliedView { state, membership }));
        let members = MemberView::default();
        let transport = transport(members.clone());
        let driver = Driver::start(
            engine,
            effects,
            RuntimeStorage {
                inner: storage,
                applied: applied.clone(),
            },
            transport,
            seeded_jitter(),
            driver_config,
        );
        staging.spawn_expiry();
        let status = driver.watch();
        Ok(Self {
            inner: Arc::new(Inner {
                driver: std::sync::Mutex::new(Some(Arc::new(driver))),
                rpc: std::sync::Mutex::new(None),
                staging: std::sync::Mutex::new(Some(staging)),
                status,
                applied,
                members,
                background: std::sync::Mutex::new(Vec::new()),
                membership_submission: tokio::sync::Mutex::new(()),
                node_id,
                cluster_id: cluster_id.to_string(),
                lease_sequence: AtomicU64::new(0),
            }),
        })
    }

    /// Starts one production node over the authenticated generated contracts.
    ///
    /// # Errors
    /// Returns legacy-store, storage or recovery failures.
    pub async fn start_endpoint<C>(
        state_dir: &Path,
        cluster_id: &str,
        identity: &crate::membership::NodeIdentity,
        caller: Arc<C>,
        provisioning: Provisioning,
        admission: &PreflightAdmission,
    ) -> Result<Self, ConsensusRuntimeError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
    {
        Self::start_endpoint_with(
            state_dir,
            cluster_id,
            identity,
            caller,
            provisioning,
            admission,
            DriverConfig::default(),
        )
        .await
    }

    async fn start_endpoint_with<C>(
        state_dir: &Path,
        cluster_id: &str,
        identity: &crate::membership::NodeIdentity,
        caller: Arc<C>,
        provisioning: Provisioning,
        admission: &PreflightAdmission,
        driver_config: DriverConfig,
    ) -> Result<Self, ConsensusRuntimeError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
    {
        let node_id = *identity.node_id();
        let incarnation = uuid::Uuid::new_v4().as_u64_pair().0;
        let outbound = caller.clone();
        let node = Self::start_with(
            state_dir,
            cluster_id,
            node_id,
            provisioning,
            admission,
            (endpoint_config(), driver_config),
            |members| {
                Arc::new(PeerBoundedTransport::new(Arc::new(WireTransport::new(
                    outbound,
                    node_id,
                    members,
                    incarnation,
                ))))
            },
        )
        .await?;
        node.refresh_members(caller.as_ref());
        let driver = node
            .driver()
            .map_err(|error| ConsensusRuntimeError::Fatal(error.to_string()))?;
        let staging = node
            .inner
            .staging
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| ConsensusRuntimeError::Fatal("consensus node stopped".into()))?;
        let service = Arc::new(WireService::new(
            caller.clone(),
            node_id,
            node.inner.members.clone(),
            driver,
            staging,
        ));
        *node
            .inner
            .rpc
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(RpcProviders {
            rpc_v1: service.clone(),
            rpc_v2: service.clone(),
            transfer: service.clone(),
            snapshot: service,
        });
        let refresher = node.clone();
        let task = tokio::spawn(async move {
            loop {
                let changed = refresher.inner.applied.changed.notified();
                tokio::select! {
                    () = changed => {}
                    () = tokio::time::sleep(MEMBER_REFRESH_INTERVAL) => {}
                }
                if refresher
                    .inner
                    .driver
                    .lock()
                    .map_or(true, |driver| driver.is_none())
                {
                    return;
                }
                refresher.refresh_members(caller.as_ref());
            }
        });
        node.inner
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(task);
        Ok(node)
    }

    /// Peer credentials for the wire adapter: committed (applied) member records
    /// win; locally enrolled records cover peers whose records have not been
    /// replicated yet (a joining learner, or this node before it catches up).
    fn refresh_members(&self, caller: &impl crate::ClusterRuntimeOps) {
        let mut merged = crate::membership::local_members(caller)
            .map(|list| {
                list.members
                    .into_iter()
                    .map(|member| (member.node_id.clone(), member))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        merged.extend(self.inner.applied.get().state.members);
        self.inner.members.update(
            self.inner.cluster_id.clone(),
            merged.into_values().collect(),
        );
    }

    /// The node's inbound consensus RPC providers, while it runs.
    ///
    /// # Errors
    /// Reports a node without a wire service (stopped, or test transport).
    pub fn rpc(&self) -> Result<RpcProviders, String> {
        self.inner
            .rpc
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "consensus node is not serving peer RPCs".to_string())
    }

    fn driver(&self) -> Result<Arc<Driver<AppliedView>>, ConsensusWriteError> {
        self.inner
            .driver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| ConsensusWriteError::Fatal("consensus node stopped".into()))
    }

    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.inner.node_id
    }

    #[must_use]
    pub fn cluster_id(&self) -> &str {
        &self.inner.cluster_id
    }

    /// The engine's current status (term, leader, commit, applied, ...).
    #[must_use]
    pub fn status(&self) -> Status {
        self.inner.status.borrow().clone()
    }

    /// The engine status as it changes.
    #[must_use]
    pub fn status_watch(&self) -> tokio::sync::watch::Receiver<Status> {
        self.inner.status.clone()
    }

    #[must_use]
    pub fn current_term(&self) -> u64 {
        self.status().term.0
    }

    /// Whether this node currently believes it leads.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.status().leader == Some(self.inner.node_id)
    }

    /// The control term and a lease sequence above every one this node issued
    /// before, for a worker lease. A worker keeps the highest (term, sequence)
    /// it accepted per execution and rejects older authority, so leases this
    /// node issues within a term must be strictly ordered. A node leads again
    /// only in a new term, so the count need not survive a restart.
    #[must_use]
    pub fn next_lease(&self) -> (u64, u64) {
        let term = self.current_term();
        let sequence = self
            .inner
            .lease_sequence
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        (term, sequence)
    }

    fn applied_membership(&self) -> Option<StoredMembership> {
        self.inner.applied.get().membership
    }

    /// Every node record (voters and learners) of the applied membership.
    #[must_use]
    pub fn committed_member_ids(&self) -> BTreeSet<NodeId> {
        self.applied_membership()
            .map(|stored| stored.membership.nodes().keys().copied().collect())
            .unwrap_or_default()
    }

    /// The voters of the applied membership (both sets while joint).
    #[must_use]
    pub fn committed_voter_ids(&self) -> BTreeSet<NodeId> {
        self.applied_membership()
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

    /// Whether durable membership exists (founded, or replicated to a joiner).
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.applied_membership().is_some() || self.status().commit.is_some()
    }

    fn not_leader(&self, hint: Option<NodeId>) -> NotLeader {
        let leader_endpoint = hint.and_then(|leader| {
            self.applied_membership()
                .and_then(|stored| stored.membership.nodes().get(&leader).cloned())
                .or_else(|| {
                    self.inner
                        .applied
                        .get()
                        .state
                        .members
                        .get(&leader.to_string())
                        .and_then(|member| member.endpoint.clone())
                })
                .filter(|endpoint| !endpoint.is_empty())
        });
        NotLeader {
            leader_id: hint,
            leader_endpoint,
        }
    }

    fn read_error(&self, error: ClientError) -> ConsensusReadError {
        match error {
            ClientError::Rejected(Rejection::NotLeader { hint }) => {
                ConsensusReadError::NotLeader(self.not_leader(hint))
            }
            ClientError::Rejected(Rejection::LeaderNotReady) => {
                ConsensusReadError::QuorumUnavailable(
                    "leader has not committed an entry in its term yet".into(),
                )
            }
            ClientError::Rejected(rejection) => {
                ConsensusReadError::QuorumUnavailable(format!("read rejected: {rejection:?}"))
            }
            ClientError::Superseded => {
                ConsensusReadError::NotLeader(self.not_leader(self.status().leader))
            }
            ClientError::Stopped(reason) => ConsensusReadError::Fatal(stopped(reason)),
            ClientError::Storage(reason) => ConsensusReadError::Storage(reason),
        }
    }

    fn write_error(&self, error: ClientError) -> ConsensusWriteError {
        match error {
            ClientError::Rejected(Rejection::NotLeader { hint }) => {
                ConsensusWriteError::NotLeader(self.not_leader(hint))
            }
            ClientError::Rejected(Rejection::LeaderNotReady) => {
                ConsensusWriteError::QuorumUnavailable(
                    "leader has not committed an entry in its term yet".into(),
                )
            }
            ClientError::Rejected(Rejection::Exhausted) => {
                ConsensusWriteError::Fatal("consensus identity space is exhausted".into())
            }
            ClientError::Rejected(rejection) => ConsensusWriteError::Membership(format!(
                "membership change rejected: {rejection:?}"
            )),
            // The slot was taken by another leader's entry: it did not apply.
            ClientError::Superseded => {
                ConsensusWriteError::NotLeader(self.not_leader(self.status().leader))
            }
            ClientError::Stopped(reason) => ConsensusWriteError::Fatal(stopped(reason)),
            ClientError::Storage(reason) => ConsensusWriteError::Fatal(reason),
        }
    }

    /// Confirms leadership with a quorum, then returns the applied view at
    /// least as new as the confirmed commit.
    async fn confirmed_view(&self) -> Result<AppliedView, ConsensusReadError> {
        let driver = self
            .driver()
            .map_err(|error| ConsensusReadError::Fatal(error.to_string()))?;
        let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, driver.read()).await {
                Err(_) => {
                    return Err(ConsensusReadError::QuorumUnavailable(
                        "quorum did not confirm leadership before the deadline".into(),
                    ));
                }
                Ok(Ok(view)) => return Ok(view),
                // A new leader serves reads once its term's first entry commits,
                // normally within one replication round.
                Ok(Err(ClientError::Rejected(Rejection::LeaderNotReady)))
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(Err(error)) => return Err(self.read_error(error)),
            }
        }
    }

    async fn propose(&self, bytes: Vec<u8>) -> Result<Vec<u8>, ConsensusWriteError> {
        let driver = self.driver()?;
        match tokio::time::timeout(WRITE_TIMEOUT, driver.propose(bytes)).await {
            Err(_) => Err(ConsensusWriteError::QuorumUnavailable(
                "the proposal did not apply before the deadline; its outcome is unknown, retry \
                 the same CommandId"
                    .into(),
            )),
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(self.write_error(error)),
        }
    }

    /// Learner admission followed by joint-consensus promotion to exactly
    /// `voters`. Every voter must have an authenticated endpoint address.
    ///
    /// # Errors
    /// Returns leader, quorum, membership, or fatal runtime failures.
    pub async fn change_voters(
        &self,
        voters: BTreeMap<NodeId, String>,
    ) -> Result<(), ConsensusWriteError> {
        let _submission = self.inner.membership_submission.lock().await;
        self.change_voters_locked(voters, None).await
    }

    async fn change_voters_locked(
        &self,
        voters: BTreeMap<NodeId, String>,
        expected_term: Option<u64>,
    ) -> Result<(), ConsensusWriteError> {
        if let Some((node_id, _)) = voters
            .iter()
            .find(|(_, endpoint)| endpoint.trim().is_empty())
        {
            return Err(ConsensusWriteError::Membership(format!(
                "consensus voter {node_id} has no authenticated endpoint"
            )));
        }
        if voters.is_empty() {
            return Err(ConsensusWriteError::Membership(
                "consensus voter set cannot be empty".to_string(),
            ));
        }
        let view = self.confirmed_view().await?;
        let admission_term = expected_term.unwrap_or_else(|| self.current_term());
        validate_admission_term(admission_term, self.current_term())?;
        // Retained publication commands/snapshots require their decoder. Only
        // peers that negotiated the owned consensus contracts run it; endpoint
        // authentication alone does not prove a learner can consume history.
        if view.state.requires_publication_decoder() {
            let (_, members) = self.inner.members.snapshot();
            for id in voters.keys().filter(|id| **id != self.inner.node_id) {
                let negotiated = members.iter().any(|member| {
                    member.node_id == id.to_string()
                        && crate::consensus_network::supports_owned_append_v2(member)
                });
                if !negotiated {
                    return Err(ConsensusWriteError::Membership(format!(
                        "new recipient {id} has not negotiated the owned consensus protocol \
                         required to decode retained capability publications"
                    )));
                }
            }
        }
        let committed = view.membership.ok_or_else(|| {
            ConsensusWriteError::Membership("committed membership is unavailable".into())
        })?;
        let [current] = committed.membership.configurations() else {
            return Err(ConsensusWriteError::Membership(
                "a membership transition is already in progress".into(),
            ));
        };
        let current = current.clone();
        // Learner admission: records for every target voter, voting unchanged.
        let mut records = committed.membership.nodes().clone();
        if voters
            .iter()
            .any(|(id, endpoint)| records.get(id) != Some(endpoint))
        {
            records.extend(voters.iter().map(|(id, endpoint)| (*id, endpoint.clone())));
            let learners = Membership::new(
                vec![current.iter().copied().collect()],
                records.into_iter().collect(),
            )
            .map_err(|error| ConsensusWriteError::Membership(format!("{error:?}")))?;
            tracing::info!(node_id = %self.inner.node_id, "consensus learner admission started");
            self.membership_step(learners, admission_term).await?;
        }
        // Promotion: the engine commits the joint configuration, then the
        // final one. Even an unchanged voter set commits a fresh position, so
        // replayed authorizations cannot reuse its epoch.
        let target = Membership::new(
            vec![voters.keys().copied().collect()],
            voters
                .iter()
                .map(|(id, endpoint)| (*id, endpoint.clone()))
                .collect(),
        )
        .map_err(|error| ConsensusWriteError::Membership(format!("{error:?}")))?;
        tracing::info!(node_id = %self.inner.node_id, voter_count = voters.len(), "consensus voter-set commit started");
        let result = self.membership_step(target, admission_term).await;
        tracing::info!(node_id = %self.inner.node_id, succeeded = result.is_ok(), "consensus voter-set commit finished");
        result
    }

    /// Propose one membership target (retrying while a learner catches up or
    /// the previous change finishes) and wait until its final configuration
    /// is applied here.
    async fn membership_step(
        &self,
        target: Membership,
        admission_term: u64,
    ) -> Result<(), ConsensusWriteError> {
        let deadline = tokio::time::Instant::now() + MEMBERSHIP_STEP_TIMEOUT;
        let driver = self.driver()?;
        let proposed = loop {
            validate_admission_term(admission_term, self.current_term())?;
            let attempt =
                tokio::time::timeout_at(deadline, driver.change_membership(target.clone())).await;
            match attempt {
                Err(_) => {
                    return Err(ConsensusWriteError::QuorumUnavailable(
                        "membership change was not accepted before the deadline".into(),
                    ));
                }
                Ok(Ok(identity)) => break identity,
                Ok(Err(ClientError::Rejected(
                    Rejection::LearnerNotCaughtUp(_) | Rejection::MembershipChangeInProgress,
                ))) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Ok(Err(error)) => return Err(self.write_error(error)),
            }
        };
        let wanted = target.configurations()[0].clone();
        loop {
            let notified = self.inner.applied.changed.notified();
            if let Some(stored) = self.applied_membership()
                && let Some(at) = stored.introduced_at
                && at.index >= proposed.index
                && stored.membership.configurations() == [wanted.clone()]
            {
                return Ok(());
            }
            validate_admission_term(admission_term, self.current_term())?;
            if tokio::time::timeout_at(deadline, async {
                tokio::select! {
                    () = notified => {}
                    () = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            })
            .await
            .is_err()
            {
                return Err(ConsensusWriteError::QuorumUnavailable(
                    "membership change did not commit before the deadline".into(),
                ));
            }
        }
    }

    /// Applies a signed exact transition under the membership submission lock.
    /// # Errors
    /// Rejects stale authority, unsupported proofs, failed quorum and invalid plans.
    pub async fn apply_voter_transition(
        &self,
        authorization: &bmux_cluster_plugin_api::cluster_types::ConsensusVoterChangeAuthorization,
        proposed: Vec<bmux_cluster_plugin_api::cluster_types::ClusterMember>,
    ) -> Result<(), String> {
        let _submission = self.inner.membership_submission.lock().await;
        let view = self
            .confirmed_view()
            .await
            .map_err(|error| error.to_string())?;
        let expected_term = self.current_term();
        let committed = view
            .membership
            .as_ref()
            .ok_or("committed membership is unavailable")?;
        let [voters] = committed.membership.configurations() else {
            return Err("a membership transition is already in progress".into());
        };
        if !voters
            .iter()
            .any(|id| id.to_string() == authorization.actor_node_id)
        {
            return Err("voter-change signer is not a current committed voter".into());
        }
        let epoch =
            membership_epoch(committed).ok_or("committed membership epoch is unavailable")?;
        let current = view.state.members.values().cloned().collect::<Vec<_>>();
        crate::consensus_membership::verify_voter_change_authorization(
            authorization,
            &self.inner.cluster_id,
            &epoch,
            &current,
            &proposed,
        )?;
        if authorization.current_members_digest == authorization.proposed_members_digest {
            // Successful join retry: quorum and signature were checked above.
            // A metadata entry alone is insufficient evidence of promotion.
            if !voters
                .iter()
                .any(|id| id.to_string() == authorization.target_node_id)
            {
                return Err("acknowledged target is not a committed voter".into());
            }
            return Ok(());
        }
        let plan =
            crate::consensus_membership::validate_membership_transition(&current, &proposed)?;
        crate::consensus_membership::preflight_publication(
            view.state.clone(),
            &authorization.actor_node_id,
            &proposed,
        )?;
        self.change_voters_locked(plan.voters, Some(expected_term))
            .await
            .map_err(|error| error.to_string())?;
        crate::consensus_membership::publish_members_locked(
            self.clone(),
            &authorization.actor_node_id,
            proposed,
        )
        .await
    }

    /// Executes a generated control mutation and returns its typed response.
    ///
    /// # Errors
    /// Returns structured leader, quorum, rejection, runtime, or internal errors.
    pub async fn mutate(
        &self,
        command: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> Result<ControlResponse, ControlServiceError> {
        let _submission = self.inner.membership_submission.lock().await;
        self.mutate_locked(command).await
    }

    pub(crate) async fn mutate_locked(
        &self,
        command: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> Result<ControlResponse, ControlServiceError> {
        let command_id = command.command_id.clone();
        let encoded = crate::control_codec::encode_control_command(&command);
        Self::decode_write_response(&command_id, self.write_locked(encoded).await)
    }

    /// Applies explicitly negotiated aggregate admission before encoding.
    ///
    /// # Errors
    /// Returns leader/quorum errors, missing activation, invalid fields or an
    /// oversized command without proposing it.
    pub async fn mutate_bounded(
        &self,
        command: bmux_cluster_plugin_api::cluster_types::ControlCommand,
    ) -> Result<ControlResponse, ControlServiceError> {
        let state = self
            .linearizable_control_state()
            .await
            .map_err(control_read_error)?;
        if !state
            .activated_features
            .contains("bounded-control-command-v1")
        {
            return Err(ControlServiceError::Rejected {
                error:
                    bmux_cluster_plugin_api::cluster_types::ControlCommandError::InvalidTransition {
                        reason: "bounded-control-command-v1 is not activated".into(),
                    },
            });
        }
        crate::control_codec::control_command_encoded_len(&command, 1_048_576).map_err(
            |error| ControlServiceError::Rejected {
                error:
                    bmux_cluster_plugin_api::cluster_types::ControlCommandError::InvalidTransition {
                        reason: format!("bounded canonical command rejected: {error}"),
                    },
            },
        )?;
        self.mutate(command).await
    }

    /// Activates one durable schema feature after membership eligibility checks.
    ///
    /// # Errors
    /// Returns structured leader, quorum, rejection, runtime, or internal errors.
    pub async fn activate_feature(
        &self,
        command: crate::control_codec::FeatureActivationCommand,
    ) -> Result<ControlResponse, ControlServiceError> {
        let command_id = command.command_id.clone();
        let encoded = crate::control_codec::encode_feature_activation(&command);
        Self::decode_write_response(&command_id, self.write(encoded).await)
    }

    /// Publishes supported capabilities for an isolated single-member authority.
    /// Multi-member publication requires reconnect-bound decoder negotiation first.
    /// # Errors
    /// Rejects incomplete authority, incompatible evidence and failed durable writes.
    pub async fn publish_capabilities(
        &self,
        reports: Vec<bmux_cluster_plugin_api::cluster_capability_types::CapabilityReport>,
    ) -> Result<ControlResponse, ControlServiceError> {
        let _submission = self.inner.membership_submission.lock().await;
        let view = self.confirmed_view().await.map_err(control_read_error)?;
        let isolated = view.membership.as_ref().filter(|stored| {
            stored.membership.nodes().len() == 1
                && stored.membership.configurations() == [BTreeSet::from([self.inner.node_id])]
        });
        let Some(membership) = isolated else {
            return Err(ControlServiceError::RuntimeUnavailable { reason: "publication requires an isolated single-member authority until peer decoder negotiation is available".into() });
        };
        let command = crate::capability_publication::PublicationCommand {
            reports,
            verified_at_unix_ms: crate::now_unix_ms(),
        };
        let bytes = command
            .encode()
            .map_err(|reason| ControlServiceError::RuntimeUnavailable { reason })?;
        let mut state = view.state.clone();
        state
            .apply_owned_publication(&command, membership)
            .map_err(|reason| ControlServiceError::Rejected {
                error:
                    bmux_cluster_plugin_api::cluster_types::ControlCommandError::InvalidTransition {
                        reason,
                    },
            })?;
        // No remote recipient exists and membership changes share this lock.
        // Do not use the raw writer, which deliberately rejects publication.
        let result = self.propose(bytes).await;
        Self::decode_write_response(&command.reports[0].command_id, result)
    }

    /// Submits a signed protocol refresh using quorum-confirmed authority.
    /// The caller supplies proofs, never the time used to validate credentials.
    /// Apply rechecks preconditions in log order and persistence precedes the reply.
    ///
    /// # Errors
    /// Rejects missing quorum, inactive features, invalid proofs and failed commits.
    pub async fn refresh_member_protocol(
        &self,
        command_id: bmux_cluster_plugin_api::cluster_types::CommandId,
        replacement: bmux_cluster_plugin_api::cluster_types::ClusterMember,
        expected_serial: String,
        expected_revision: u64,
        node_signature: Vec<u8>,
    ) -> Result<ControlResponse, ControlServiceError> {
        let mut state = self
            .linearizable_control_state()
            .await
            .map_err(control_read_error)?;
        let command = crate::control_codec::ProtocolRefreshCommand {
            command_id: command_id.value,
            replacement,
            expected_serial,
            expected_revision,
            node_signature,
            verified_at_unix_ms: crate::now_unix_ms(),
        };
        let encoded = command
            .encode()
            .map_err(|error| ControlServiceError::Internal {
                reason: error.to_string(),
            })?;
        let preflight = state.apply_refresh_command(&command);
        if let bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Rejected { error } =
            preflight.result
        {
            return Err(ControlServiceError::Rejected { error });
        }
        Self::decode_write_response(&command_id, self.write(encoded).await)
    }

    /// Validates bootstrap proof using current authority and submits it durably.
    /// Verification time is assigned here, never accepted from a service caller.
    ///
    /// # Errors
    /// Rejects missing quorum, invalid proof/preconditions and failed commits.
    pub async fn bootstrap_principal(
        &self,
        proof: bmux_cluster_plugin_api::cluster_principal_bootstrap_types::BootstrapProof,
        expected_control_revision: u64,
    ) -> Result<ControlResponse, ControlServiceError> {
        let view = self.confirmed_view().await.map_err(control_read_error)?;
        let command = crate::principal_bootstrap::BootstrapCommand {
            proof,
            expected_control_revision,
            verified_at_unix_ms: crate::now_unix_ms(),
        };
        // The preflight uses one confirmed state/membership view. Apply checks
        // again in log order, so intervening membership or revision changes reject.
        let membership = view
            .membership
            .as_ref()
            .ok_or_else(|| ControlServiceError::Rejected {
                error:
                    bmux_cluster_plugin_api::cluster_types::ControlCommandError::InvalidTransition {
                        reason: "bootstrap requires committed membership".into(),
                    },
            })?;
        let mut state = view.state.clone();
        let preflight = state.apply_owned_bootstrap_command(&command, membership);
        if let bmux_cluster_plugin_api::cluster_types::ControlCommandResult::Rejected { error } =
            preflight.result
        {
            return Err(ControlServiceError::Rejected { error });
        }
        let encoded = command
            .encode()
            .map_err(|reason| ControlServiceError::Internal { reason })?;
        Self::decode_write_response(
            &command.proof.statement.command_id,
            self.write(encoded).await,
        )
    }

    fn decode_write_response(
        command_id: &bmux_cluster_plugin_api::cluster_types::CommandId,
        result: Result<Vec<u8>, ConsensusWriteError>,
    ) -> Result<ControlResponse, ControlServiceError> {
        match result {
            Ok(response) => {
                let response =
                    crate::control_codec::decode_control_response(&response).map_err(|error| {
                        ControlServiceError::Internal {
                            reason: format!("committed control response is invalid: {error}"),
                        }
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
    /// Returns structured leader, quorum, runtime, or storage errors.
    pub async fn read_linearizable_view(&self) -> Result<ControlStateView, ControlServiceError> {
        let view = self.confirmed_view().await.map_err(control_read_error)?;
        let mut state_view = view.state.to_view(ControlReadConsistency::Linearizable);
        state_view.membership_epoch = view.membership.as_ref().and_then(membership_epoch);
        Ok(state_view)
    }

    /// Returns an explicitly stale generated state view.
    ///
    /// # Errors
    /// Never fails today; kept fallible for callers that treat it as a read.
    pub fn read_stale_view(&self) -> Result<ControlStateView, ControlServiceError> {
        Ok(self
            .persisted_control_state()
            .to_view(ControlReadConsistency::Stale))
    }

    /// Submits one canonical control command to the current leader and returns
    /// the canonical response bytes produced by its application.
    ///
    /// # Errors
    /// Returns actionable leader guidance, quorum unavailability, membership
    /// errors, or fatal runtime failures. A command is never replayed internally
    /// after dispatch.
    pub async fn write(&self, command: Vec<u8>) -> Result<Vec<u8>, ConsensusWriteError> {
        let _submission = self.inner.membership_submission.lock().await;
        self.write_locked(command).await
    }

    async fn write_locked(&self, command: Vec<u8>) -> Result<Vec<u8>, ConsensusWriteError> {
        // Publication has a decoder, but no membership-serialized proposal path
        // yet. Never let the generic raw writer bypass that compatibility boundary:
        // apply-time rejection cannot protect an older recipient's decoder.
        if command.starts_with(b"BMCAP001") {
            return Err(ConsensusWriteError::Membership(
                "capability publication requires membership-safe negotiated submission".into(),
            ));
        }
        // Loss of quorum never authorizes a mutation: confirm first.
        self.confirmed_view().await?;
        self.propose(command).await
    }

    /// Confirms leadership and quorum before returning the applied state.
    ///
    /// # Errors
    /// Returns actionable leader guidance, quorum unavailability, fatal
    /// failures, or durable state read failures.
    pub async fn linearizable_control_state(&self) -> Result<ControlState, ConsensusReadError> {
        self.confirmed_view().await.map(|view| view.state)
    }

    /// The durably applied state without a quorum confirmation.
    ///
    /// Callers must explicitly label this result stale when presenting it.
    #[must_use]
    pub fn persisted_control_state(&self) -> ControlState {
        self.inner.applied.get().state
    }

    /// The control revision applied now.
    #[must_use]
    pub fn control_revision(&self) -> u64 {
        *self.inner.applied.revision.borrow()
    }

    /// The applied control revision as it changes.
    #[must_use]
    pub fn revision_watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.applied.revision.subscribe()
    }

    /// Stops admission, waits for in-flight calls, and releases durable
    /// ownership of the store.
    ///
    /// # Errors
    /// Reports in-flight calls that outlived the bounded wait (durable
    /// ownership is then released when they finish).
    pub async fn shutdown(&self) -> Result<(), String> {
        let Some(mut driver) = self
            .inner
            .driver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return Ok(());
        };
        self.inner
            .rpc
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        for task in self
            .inner
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        let driver = loop {
            match Arc::try_unwrap(driver) {
                Ok(driver) => break driver,
                Err(shared) if tokio::time::Instant::now() < deadline => {
                    driver = shared;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(_) => {
                    return Err("in-flight consensus calls outlived the shutdown deadline".into());
                }
            }
        };
        // No wire service remains (each held the driver), so no transfer can
        // be admitted: discard staged objects before the store closes.
        let staging = self
            .inner
            .staging
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(staging) = staging {
            let closed = tokio::task::spawn_blocking(move || staging.close()).await;
            if let Ok(Err(error)) = closed {
                // Staging is disposable: the next start discards leftovers.
                tracing::warn!(%error, "owned snapshot staging cleanup was incomplete");
            }
        }
        driver.shutdown().await;
        Ok(())
    }
}

fn stopped(reason: Option<StopReason>) -> String {
    reason.map_or_else(
        || "consensus engine stopped".to_string(),
        |reason| format!("consensus engine stopped: {reason:?}"),
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
        ConsensusReadError::Fatal(reason) | ConsensusReadError::Storage(reason) => {
            ControlServiceError::Internal { reason }
        }
    }
}
