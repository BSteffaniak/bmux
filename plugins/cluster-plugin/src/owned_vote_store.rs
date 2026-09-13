//! Transactional owned vote persistence. Blocking calls belong on a bounded
//! storage executor; this component does not activate a consensus runtime.
use crate::consensus_model::{
    AppendDecision, AppendOutcome, AppendReconciliation, AppendRequest, Entry, EntryPayload,
    HardState, LogIdentity, Membership, RequestVote, Term, VoteGateError, VoteResponse,
};
#[cfg(test)]
use crate::consensus_model::{VoteAction, VoteGate};
use crate::membership::NodeId;
use redb::ReadableTableMetadata;
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, StorageBackend, TableDefinition,
};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_authority");
const EMPTY_POSITIONS: [&str; 5] = [
    "commit",
    "applied",
    "snapshot",
    "membership",
    "applied_membership",
];
const FORMAT: &[u8] = b"BMOWNV05";
const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("owned_log");
const MEMBERSHIPS: TableDefinition<u64, &[u8]> = TableDefinition::new("owned_memberships");
const HEADER_LEN: u64 = 4096;
const HEADER_BYTES: usize = 4096;
const HEADER_MAGIC: &[u8; 8] = b"BMOWNDB1";

/// A locked inode containing an immutable identification header and redb payload.
/// Offsets exposed to redb never include the header.
#[derive(Debug)]
struct OwnedBackend {
    file: std::sync::Mutex<File>,
    status: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl OwnedBackend {
    fn initialize(mut file: File, cluster: &str) -> io::Result<Self> {
        file.try_lock().map_err(io::Error::other)?;
        let header = owned_header(cluster)?;
        file.write_all(&header)?;
        file.sync_all()?;
        Ok(Self {
            file: std::sync::Mutex::new(file),
            status: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        })
    }

    fn open(path: &Path, cluster: &str) -> io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(io::Error::other)?;
        let mut header = [0; HEADER_BYTES];
        file.read_exact(&mut header)?;
        if header != owned_header(cluster)? {
            return Err(invalid(
                "unsupported, corrupt or wrong-cluster owned header",
            ));
        }
        if file.metadata()?.len() <= HEADER_LEN {
            return Err(invalid("incomplete owned database"));
        }
        Ok(Self {
            file: std::sync::Mutex::new(file),
            status: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        })
    }

    fn operate<T>(&self, operation: impl FnOnce(&mut File) -> io::Result<T>) -> io::Result<T> {
        let mut file = self.file.lock().map_err(|_| {
            self.status.store(2, std::sync::atomic::Ordering::SeqCst);
            io::Error::other("owned backend poisoned")
        })?;
        if self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned backend stopped"));
        }
        let result = operation(&mut file);
        if result.is_err() {
            self.status.store(2, std::sync::atomic::Ordering::SeqCst);
        }
        // Publish failure before another operation can acquire this mutex.
        drop(file);
        result
    }
}

// This prototype stores only one vote, not the future consensus log. Reject
// oversized files explicitly rather than doing unbounded preflight copying.
const MAX_VOTE_PAYLOAD: u64 = 64 * 1024 * 1024;
// Until compaction is integrated, stop accepting growth at a fixed log budget.
// This also bounds suffix removal work in one transaction.
const MAX_RETAINED_ENTRIES: u64 = 4096;

// Bound aggregate scratch use in this process, not just each file. Never queue
// startup attempts behind slow recovery; the caller may retry explicit Busy.
static PREFLIGHT_ADMISSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn admit_preflight() -> io::Result<std::sync::MutexGuard<'static, ()>> {
    PREFLIGHT_ADMISSION.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "owned preflight busy")
        }
        std::sync::TryLockError::Poisoned(_) => {
            io::Error::other("owned preflight stopped after panic")
        }
    })
}

fn preflight_payload(backend: &OwnedBackend, cluster: &str) -> io::Result<()> {
    let admission = admit_preflight()?;
    let scratch = tempfile::tempfile()?;
    let scratch_backend = OwnedBackend::initialize(scratch, cluster)?;
    backend.operate(|file| {
        let len = file
            .metadata()?
            .len()
            .checked_sub(HEADER_LEN)
            .ok_or_else(|| invalid("missing payload"))?;
        if len == 0 || len > MAX_VOTE_PAYLOAD {
            return Err(invalid("owned vote payload exceeds supported size"));
        }
        file.seek(SeekFrom::Start(HEADER_LEN))?;
        scratch_backend.operate(|scratch| {
            scratch.seek(SeekFrom::Start(HEADER_LEN))?;
            if io::copy(&mut file.take(len), scratch)? != len {
                return Err(invalid("partial preflight copy"));
            }
            Ok(())
        })
    })?;
    let status = scratch_backend.status.clone();
    let database = Database::builder()
        .create_with_backend(scratch_backend)
        .map_err(io::Error::other)?;
    let validation = read_vote(&database, cluster);
    drop(database);
    if status.load(std::sync::atomic::Ordering::SeqCst) != 1 {
        return Err(io::Error::other("owned preflight cleanup failed"));
    }
    drop(admission);
    validation.map(|_| ())
}

fn read_vote(database: &Database, cluster: &str) -> io::Result<HardState> {
    let transaction = database.begin_read().map_err(io::Error::other)?;
    let table = transaction.open_table(TABLE).map_err(io::Error::other)?;
    if table
        .get("format")
        .map_err(io::Error::other)?
        .ok_or_else(|| invalid("missing format"))?
        .value()
        != FORMAT
    {
        return Err(invalid("unsupported owned format"));
    }
    if table
        .get("cluster")
        .map_err(io::Error::other)?
        .ok_or_else(|| invalid("missing cluster"))?
        .value()
        != cluster.as_bytes()
    {
        return Err(invalid("cluster mismatch"));
    }
    validate_log_recovery(&transaction)?;
    decode(
        table
            .get("vote")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing vote"))?
            .value(),
    )
}

fn application_membership_bytes(meta: &redb::Table<&str, &[u8]>) -> io::Result<Vec<u8>> {
    Ok(meta
        .get("applied_membership")
        .map_err(io::Error::other)?
        .ok_or_else(|| invalid("missing applied membership"))?
        .value()
        .to_vec())
}

fn compact_table(
    tx: &redb::WriteTransaction,
    definition: TableDefinition<u64, &[u8]>,
    boundary: u64,
) -> io::Result<()> {
    let table = tx.open_table(definition).map_err(io::Error::other)?;
    let mut suffix = Vec::new();
    let mut retained_bytes = 0u64;
    for record in table
        .range((
            std::ops::Bound::Excluded(boundary),
            std::ops::Bound::Unbounded,
        ))
        .map_err(io::Error::other)?
    {
        let (index, bytes) = record.map_err(io::Error::other)?;
        retained_bytes = retained_bytes
            .checked_add(u64::try_from(bytes.value().len()).map_err(io::Error::other)?)
            .ok_or_else(|| invalid("compaction byte overflow"))?;
        if retained_bytes > MAX_VOTE_PAYLOAD
            || u64::try_from(suffix.len()).map_err(io::Error::other)? >= MAX_RETAINED_ENTRIES
        {
            return Err(invalid("compaction suffix exceeds supported bounds"));
        }
        suffix.push((index.value(), bytes.value().to_vec()));
    }
    // Rebuild only the retained suffix: per-entry prefix deletion causes excessive
    // copy-on-write growth. The old tree remains recoverable until this commit.
    drop(table);
    tx.delete_table(definition).map_err(io::Error::other)?;
    let mut table = tx.open_table(definition).map_err(io::Error::other)?;
    for (index, bytes) in suffix {
        table
            .insert(index, bytes.as_slice())
            .map_err(io::Error::other)?;
    }
    Ok(())
}

fn write_position(meta: &redb::Table<&str, &[u8]>, key: &str) -> io::Result<Option<LogIdentity>> {
    decode_position(
        meta.get(key)
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing position"))?
            .value(),
    )
}

fn retained_after_replace(
    log: &redb::Table<u64, &[u8]>,
    first: u64,
    incoming: usize,
) -> io::Result<u64> {
    let mut count = u64::try_from(incoming).map_err(io::Error::other)?;
    for entry in log.range(..first).map_err(io::Error::other)? {
        entry.map_err(io::Error::other)?;
        count += 1;
        if count > MAX_RETAINED_ENTRIES {
            break;
        }
    }
    Ok(count)
}

fn validate_append_admission(request: &AppendRequest, limits: (usize, usize)) -> io::Result<()> {
    request
        .validate(limits.0, limits.1)
        .map_err(|_| invalid("invalid append request"))?;
    Ok(())
}

fn encode_position(position: Option<LogIdentity>) -> Vec<u8> {
    let Some(position) = position else {
        return Vec::new();
    };
    let mut bytes = Vec::with_capacity(48);
    bytes.extend_from_slice(&position.term.0.to_le_bytes());
    bytes.extend_from_slice(position.leader.as_bytes());
    bytes.extend_from_slice(&position.index.0.to_le_bytes());
    bytes
}

fn decode_position(bytes: &[u8]) -> io::Result<Option<LogIdentity>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() != 48 {
        return Err(invalid("corrupt owned position"));
    }
    let leader = NodeId::from_bytes(bytes[8..40].try_into().map_err(io::Error::other)?);
    leader.public_key().map_err(io::Error::other)?;
    Ok(Some(LogIdentity {
        term: Term(u64::from_le_bytes(
            bytes[..8].try_into().map_err(io::Error::other)?,
        )),
        leader,
        index: crate::consensus_model::LogIndex(u64::from_le_bytes(
            bytes[40..].try_into().map_err(io::Error::other)?,
        )),
    }))
}

// This format supports uncompacted logs only. Nonempty snapshot metadata is an
// explicit unsupported recovery state until snapshot installation is integrated.
fn validate_control_recovery(meta: &redb::ReadOnlyTable<&str, &[u8]>) -> io::Result<()> {
    let control = crate::control_state::ControlState::decode_snapshot(
        meta.get("control")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing control state"))?
            .value(),
    )
    .map_err(io::Error::other)?;
    if control.cluster_id.as_bytes()
        != meta
            .get("cluster")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing cluster"))?
            .value()
    {
        return Err(invalid("application cluster mismatch"));
    }
    Ok(())
}

fn validate_published_snapshot(
    meta: &redb::ReadOnlyTable<&str, &[u8]>,
    applied: Option<LogIdentity>,
) -> io::Result<()> {
    if let Some(bytes) = meta.get("published_snapshot").map_err(io::Error::other)? {
        let (descriptor, _) = crate::consensus_storage::decode_owned_snapshot(bytes.value())
            .map_err(io::Error::other)?;
        let cluster = meta
            .get("cluster")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing cluster"))?;
        if descriptor.cluster_id.as_bytes() != cluster.value() {
            return Err(invalid("published snapshot cluster mismatch"));
        }
        crate::consensus_model::validate_position_prefix(descriptor.last_included, applied)
            .map_err(|_| invalid("published snapshot beyond applied state"))?;
    }
    Ok(())
}

fn recovered_snapshot_membership(
    meta: &redb::ReadOnlyTable<&str, &[u8]>,
    snapshot: Option<LogIdentity>,
    applied: Option<LogIdentity>,
) -> io::Result<Vec<u8>> {
    if snapshot.is_none() {
        return Ok(Vec::new());
    }
    let bytes = meta
        .get("compacted_snapshot")
        .map_err(io::Error::other)?
        .ok_or_else(|| invalid("missing compacted snapshot"))?;
    let (descriptor, _) =
        crate::consensus_storage::decode_owned_snapshot(bytes.value()).map_err(io::Error::other)?;
    let cluster = meta
        .get("cluster")
        .map_err(io::Error::other)?
        .ok_or_else(|| invalid("missing cluster"))?;
    if descriptor.last_included != snapshot || descriptor.cluster_id.as_bytes() != cluster.value() {
        return Err(invalid("snapshot boundary or cluster mismatch"));
    }
    crate::consensus_model::validate_position_prefix(snapshot, applied)
        .map_err(|_| invalid("snapshot beyond applied"))?;
    let membership = descriptor
        .membership
        .ok_or_else(|| invalid("missing snapshot membership"))?;
    crate::consensus_storage::encode_owned_entry(&Entry {
        identity: membership
            .introduced_at
            .ok_or_else(|| invalid("unpositioned snapshot membership"))?,
        payload: EntryPayload::Membership(membership.membership),
    })
    .map_err(io::Error::other)
}

fn validate_log_recovery(transaction: &redb::ReadTransaction) -> io::Result<()> {
    let meta = transaction.open_table(TABLE).map_err(io::Error::other)?;
    let position = |key| -> io::Result<Option<LogIdentity>> {
        decode_position(
            meta.get(key)
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("missing owned recovery position"))?
                .value(),
        )
    };
    let commit = position("commit")?;
    let applied = position("applied")?;
    validate_control_recovery(&meta)?;
    validate_published_snapshot(&meta, applied)?;
    let applied_membership = meta
        .get("applied_membership")
        .map_err(io::Error::other)?
        .ok_or_else(|| invalid("missing applied membership"))?;
    let snapshot = position("snapshot")?;
    let seed = recovered_snapshot_membership(&meta, snapshot, applied)?;
    crate::consensus_model::validate_position_prefix(applied, commit)
        .map_err(|_| invalid("invalid applied/commit positions"))?;
    let log = transaction.open_table(LOG).map_err(io::Error::other)?;
    let memberships = transaction
        .open_table(MEMBERSHIPS)
        .map_err(io::Error::other)?;
    if log.len().map_err(io::Error::other)? > MAX_RETAINED_ENTRIES {
        return Err(invalid("owned retained log limit exceeded"));
    }
    let mut previous: Option<LogIdentity> = snapshot;
    let mut latest_membership = seed.clone();
    let mut latest_applied_membership = seed;
    let mut membership_count = 0;
    for record in log.iter().map_err(io::Error::other)? {
        let (index, bytes) = record.map_err(io::Error::other)?;
        let entry = crate::consensus_storage::decode_owned_entry(bytes.value())
            .map_err(io::Error::other)?;
        if entry.identity.index.0 != index.value()
            || previous.is_some_and(|previous| {
                previous.index.0.checked_add(1) != Some(index.value())
                    || previous.term > entry.identity.term
            })
            || previous.is_none() && index.value() != 0
        {
            return Err(invalid("inconsistent owned log"));
        }
        if matches!(entry.payload, EntryPayload::Membership(_)) {
            let indexed = memberships
                .get(index.value())
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("missing membership index"))?;
            if indexed.value() != bytes.value() {
                return Err(invalid("conflicting membership index"));
            }
            latest_membership = bytes.value().to_vec();
            if applied.is_some_and(|applied| entry.identity.index <= applied.index) {
                latest_applied_membership.clone_from(&latest_membership);
            }
            membership_count += 1;
        }
        previous = Some(entry.identity);
    }
    if applied_membership.value() != latest_applied_membership {
        return Err(invalid("applied membership mismatch"));
    }
    if memberships.len().map_err(io::Error::other)? != membership_count {
        return Err(invalid("orphan membership index"));
    }
    if meta
        .get("membership")
        .map_err(io::Error::other)?
        .ok_or_else(|| invalid("missing effective membership"))?
        .value()
        != latest_membership
    {
        return Err(invalid("inconsistent effective membership"));
    }
    for position in [commit, applied].into_iter().flatten() {
        if Some(position) == snapshot {
            continue;
        }
        let bytes = log
            .get(position.index.0)
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("position outside durable log"))?;
        if crate::consensus_storage::decode_owned_entry(bytes.value())
            .map_err(io::Error::other)?
            .identity
            != position
        {
            return Err(invalid("position identity mismatch"));
        }
    }
    Ok(())
}

fn owned_header(cluster: &str) -> io::Result<[u8; HEADER_BYTES]> {
    if cluster.is_empty() || cluster.len() > 1024 {
        return Err(invalid("invalid cluster identity"));
    }
    let mut header = [0; HEADER_BYTES];
    header[..8].copy_from_slice(HEADER_MAGIC);
    header[8..16].copy_from_slice(&(cluster.len() as u64).to_le_bytes());
    header[16..16 + cluster.len()].copy_from_slice(cluster.as_bytes());
    let digest = Sha256::digest(&header[..HEADER_BYTES - 32]);
    header[HEADER_BYTES - 32..].copy_from_slice(&digest);
    Ok(header)
}

impl StorageBackend for OwnedBackend {
    fn len(&self) -> io::Result<u64> {
        self.operate(|file| {
            file.metadata()?
                .len()
                .checked_sub(HEADER_LEN)
                .ok_or_else(|| invalid("truncated owned header"))
        })
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.operate(|file| {
            check_payload_range(offset, out.len())?;
            file.seek(SeekFrom::Start(payload_offset(offset)?))?;
            file.read_exact(out)
        })
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.operate(|file| {
            check_payload_range(len, 0)?;
            file.set_len(payload_offset(len)?)
        })
    }

    fn sync_data(&self) -> io::Result<()> {
        self.operate(|file| file.sync_all())
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.operate(|file| {
            check_payload_range(offset, data.len())?;
            file.seek(SeekFrom::Start(payload_offset(offset)?))?;
            file.write_all(data)
        })
    }

    fn close(&self) -> io::Result<()> {
        self.operate(|file| {
            file.sync_all()?;
            self.status.store(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    }
}

#[cfg(test)]
fn initialization_crash(point: &str) {
    if std::env::var("BMUX_OWNED_INIT_CRASH").as_deref() == Ok(point) {
        std::process::exit(73);
    }
}

fn check_payload_range(offset: u64, len: usize) -> io::Result<()> {
    let end = offset
        .checked_add(u64::try_from(len).map_err(io::Error::other)?)
        .ok_or_else(|| invalid("owned payload range overflow"))?;
    if end > MAX_VOTE_PAYLOAD {
        return Err(invalid("owned vote payload exceeds supported size"));
    }
    Ok(())
}

fn payload_offset(offset: u64) -> io::Result<u64> {
    offset
        .checked_add(HEADER_LEN)
        .ok_or_else(|| invalid("owned offset overflow"))
}

#[derive(Debug)]
pub struct OwnedVoteStore {
    database: Database,
    status: std::sync::Arc<std::sync::atomic::AtomicU8>,
    state: HardState,
    failed: bool,
}

/// Serial storage adapter for the deterministic vote gate.
///
/// The owner must run
/// this blocking adapter on its bounded storage executor and serialize log and
/// configuration changes with it. It exposes no synthetic success completion.
#[derive(Debug)]
pub struct DurableVoteGate {
    #[cfg(test)]
    gate: VoteGate,
    store: OwnedVoteStore,
    staging: Option<(std::fs::File, crate::transfer_staging::TransferStaging)>,
}

impl DurableVoteGate {
    /// Construct only from recovered durable authority and a fresh incarnation.
    #[must_use]
    pub const fn new(incarnation: u128, store: OwnedVoteStore) -> Self {
        let _ = incarnation;
        Self {
            #[cfg(test)]
            gate: VoteGate::new(incarnation, store.state()),
            store,
            staging: None,
        }
    }

    /// Return a vote only after any required authority change commits.
    /// # Errors
    /// Storage failures permanently stop voting for this incarnation.
    #[cfg(test)]
    pub fn request(
        &mut self,
        local: NodeId,
        last_log: Option<LogIdentity>,
        membership: &Membership,
        request: RequestVote,
    ) -> Result<VoteResponse, VoteGateError> {
        if self.store.status.load(std::sync::atomic::Ordering::SeqCst) == 2 {
            return Err(VoteGateError::StorageFailure);
        }
        match self.gate.request(local, last_log, membership, request)? {
            VoteAction::Reply(response) => Ok(response),
            VoteAction::Persist { id, hard_state } => {
                let committed = self.store.persist(hard_state).is_ok();
                self.gate
                    .complete(id, committed)?
                    .ok_or(VoteGateError::Stopped)
            }
        }
    }
}

/// Single-flight asynchronous owner of durable vote and append operations.
///
/// No request queue is created. Cancellation leaves the admitted blocking
/// operation owning the store until its commit completes. Voting reads log and
/// membership from that store rather than accepting caller-provided authority.
#[derive(Debug)]
pub struct VoteExecutor {
    gate: std::sync::Arc<tokio::sync::Mutex<Option<DurableVoteGate>>>,
    closing: std::sync::atomic::AtomicBool,
}

/// Append admission errors are distinct from uncertain durable completion.
#[derive(Debug)]
pub enum AppendExecutionError {
    InvalidRequest(crate::consensus_model::AppendError),
    CompactionRequired,
    Executor(VoteGateError),
    Storage(io::Error),
}

impl VoteExecutor {
    /// Recover existing authority off the async thread without an unbounded queue.
    ///
    /// Cancellation before handoff leaves cleanup with the blocking worker.
    /// # Errors
    /// Busy recovery returns `WouldBlock`; corrupt or unsupported stores fail closed.
    pub async fn recover(
        path: std::path::PathBuf,
        cluster: String,
        incarnation: u128,
    ) -> io::Result<Self> {
        static RECOVERY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let permit = RECOVERY
            .try_acquire()
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "owned recovery busy"))?;
        let (send, receive) = tokio::sync::oneshot::channel();
        let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let store = match OwnedVoteStore::open(&path, &cluster) {
                Ok(store) => store,
                Err(error) => {
                    let _ = send.send(Err(error));
                    return;
                }
            };
            let executor = Self::new(DurableVoteGate::new(incarnation, store));
            if let Err(error) = executor.handoff(send, acknowledged) {
                tracing::error!(%error, "cancelled owned recovery cleanup failed");
            }
        });
        let executor = receive
            .await
            .map_err(|_| io::Error::other("owned recovery worker stopped"))??;
        // No suspension between accepting ownership and acknowledging handoff.
        acknowledge
            .send(())
            .map_err(|()| io::Error::other("owned recovery handoff stopped"))?;
        Ok(executor)
    }

    // Blocking-worker ownership lasts until the receiver explicitly accepts it.
    // Return cleanup failures to the worker rather than losing them in Drop.
    fn handoff(
        self,
        send: tokio::sync::oneshot::Sender<io::Result<Self>>,
        acknowledged: tokio::sync::oneshot::Receiver<()>,
    ) -> io::Result<()> {
        let owner = self.gate.clone();
        let delivered = send.send(Ok(self)).is_ok();
        if (!delivered || acknowledged.blocking_recv().is_err())
            && let Some(gate) = owner.blocking_lock().take()
        {
            gate.store.close()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn new(gate: DurableVoteGate) -> Self {
        Self {
            gate: std::sync::Arc::new(tokio::sync::Mutex::new(Some(gate))),
            closing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Admit at most one blocking operation, without waiting in a request queue.
    /// # Errors
    /// Returns Busy during another operation, or Stopped after shutdown.
    // The guard must move into the blocking task and span the durable commit;
    // releasing it at its last pre-spawn use would break single-flight ownership.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn request(
        &self,
        local: NodeId,
        request: RequestVote,
    ) -> Result<VoteResponse, VoteGateError> {
        self.run(move |gate| gate.store.vote(local, request)).await
    }

    /// Configure staging explicitly before admitting transfers. The directory
    /// must be owner-private. Empty directories receive a durable format marker;
    /// marked directories recover only validated disposable transfer files.
    /// # Errors
    /// Rejects invalid quotas, repeated configuration and busy/stopped admission.
    pub async fn configure_staging(
        &self,
        directory: std::path::PathBuf,
        peer_bytes: u64,
        total_bytes: u64,
    ) -> Result<(), String> {
        let staging = crate::transfer_staging::TransferStaging::new(peer_bytes, total_bytes)
            .map_err(|error| error.to_string())?;
        let owner = std::sync::Arc::downgrade(&self.gate);
        self.run(move |gate| {
            if gate.staging.is_some() {
                return Ok(Err("staging already configured".into()));
            }
            let directory = match crate::transfer_staging::recover_directory(&directory) {
                Ok(directory) => directory,
                Err(error) => return Ok(Err(error.to_string())),
            };
            gate.staging = Some((directory, staging));
            // Install the timer inside admitted work: cancellation of configure
            // cannot leave successfully configured staging without idle cleanup.
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let Some(owner) = owner.upgrade() else { break };
                    let Ok(mut guard) = owner.try_lock_owned() else {
                        continue;
                    };
                    let result = tokio::task::spawn_blocking(move || {
                        let Some(gate) = guard.as_mut() else {
                            return false;
                        };
                        if let Some((_, staging)) = gate.staging.as_mut()
                            && let Err(error) = staging.expire(std::time::Instant::now())
                        {
                            // Keep failed-deletion reservations for retry and
                            // stop authority rather than hide a cleanup failure.
                            gate.store.failed = true;
                            tracing::error!(%error, "owned staging expiry failed");
                        }
                        true
                    })
                    .await;
                    if !matches!(result, Ok(true)) {
                        break;
                    }
                }
            });
            Ok(Ok(()))
        })
        .await
        .map_err(|error| format!("staging configuration failed: {error:?}"))?
    }

    /// Authenticate and mutate disposable staging under the same single-flight
    /// authority as votes and appends. Offsets acknowledge staging, not replication.
    /// # Errors
    /// Rejects busy/stopped authority, wrong clusters, stale terms and non-voters.
    // Keep durable authority read, authentication and staging in one visible
    // critical section so no refactor accidentally releases admission between them.
    #[allow(clippy::too_many_lines)]
    pub async fn transfer<C: crate::ClusterRuntimeOps + Send + Sync + 'static>(
        &self,
        caller: std::sync::Arc<C>,
        local: NodeId,
        operation: String,
        request: bmux_cluster_plugin_api::cluster_consensus_transfer_types::ChunkRequest,
    ) -> Result<u64, String> {
        // Charge and validate before moving attacker-controlled data into a task.
        crate::consensus_network::transfer_signing_payload(&operation, &request)?;
        self.run(move |gate| {
            let store = &mut gate.store;
            if store.failed || store.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                return Err(VoteGateError::Stopped);
            }
            store.failed = true;
            let authority = (|| -> io::Result<_> {
                let tx = store.database.begin_read().map_err(io::Error::other)?;
                let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
                let control = meta
                    .get("control")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing control authority"))?;
                let state = crate::control_state::ControlState::decode_snapshot(control.value())
                    .map_err(io::Error::other)?;
                let cluster = meta
                    .get("cluster")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing cluster authority"))?;
                let cluster =
                    String::from_utf8(cluster.value().to_vec()).map_err(io::Error::other)?;
                let membership = meta
                    .get("membership")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing membership authority"))?;
                let membership = if membership.value().is_empty() {
                    None
                } else {
                    let entry = crate::consensus_storage::decode_owned_entry(membership.value())
                        .map_err(io::Error::other)?;
                    let EntryPayload::Membership(membership) = entry.payload else {
                        return Err(invalid("invalid membership authority"));
                    };
                    Some(membership)
                };
                Ok((state, cluster, membership))
            })()
            .map_err(|_| VoteGateError::StorageFailure)?;
            store.failed = false;
            let (state, cluster, membership) = authority;
            if request.header.cluster_id != cluster
                || request.header.leader_term != store.state.term.0
            {
                return Ok(Err(
                    "transfer cluster or term differs from owned authority".into()
                ));
            }
            let eligible = membership.as_ref().is_some_and(|membership| {
                membership.nodes().contains_key(&local)
                    && membership.configurations().iter().any(|set| {
                        set.iter()
                            .any(|id| id.as_bytes().as_slice() == request.header.sender.as_slice())
                    })
            });
            if !eligible {
                return Ok(Err(
                    "transfer peer is not eligible in owned membership".into()
                ));
            }
            let members = state.members.into_values().collect::<Vec<_>>();
            Ok((|| -> Result<u64, String> {
                let source = crate::consensus_network::authenticate_transfer_request(
                    caller.as_ref(),
                    &operation,
                    local,
                    &members,
                    &request,
                )?;
                let (directory, staging) =
                    gate.staging.as_mut().ok_or("staging is not configured")?;
                let now = std::time::Instant::now();
                staging.expire(now).map_err(|error| error.to_string())?;
                let binding: [u8; 32] = Sha256::digest(
                    bmux_plugin_sdk::encode_service_message(&request.header)
                        .map_err(|error| error.to_string())?,
                )
                .into();
                let key = crate::transfer_staging::TransferKey {
                    peer: *source.as_bytes(),
                    incarnation: request.header.sender_incarnation,
                    sequence: request.header.sequence,
                };
                if operation != "begin" {
                    staging
                        .validate_binding(key, binding)
                        .map_err(|error| error.to_string())?;
                }
                let result = match operation.as_str() {
                    "begin" => staging.begin(
                        directory,
                        key,
                        binding,
                        request.header.total_bytes,
                        request
                            .header
                            .checksum
                            .as_slice()
                            .try_into()
                            .map_err(|_| "invalid checksum")?,
                        now,
                    ),
                    "chunk" => staging.chunk(key, request.offset, &request.data, now),
                    "finish" => staging
                        .finish_validated(key, |file, total| {
                            crate::consensus_network::validate_staged_object(
                                file,
                                total,
                                &request.header,
                            )
                        })
                        .map(|()| request.header.total_bytes),
                    "abort" => staging.abort(key).map(|()| 0),
                    _ => return Err("unknown transfer operation".into()),
                };
                result.map_err(|error| error.to_string())
            })())
        })
        .await
        .map_err(|error| format!("owned transfer admission failed: {error:?}"))?
    }

    /// Serialize authenticated append with voting and shutdown.
    /// # Errors
    /// Busy admission is explicit; storage errors suppress dependent replies.
    pub async fn append(
        &self,
        request: AppendRequest,
        limits: (usize, usize),
    ) -> Result<AppendDecision, AppendExecutionError> {
        request
            .validate(limits.0, limits.1)
            .map_err(AppendExecutionError::InvalidRequest)?;
        self.run(move |gate| {
            if gate.store.failed || gate.store.status.load(std::sync::atomic::Ordering::SeqCst) != 0
            {
                return Err(VoteGateError::Stopped);
            }
            Ok(gate.store.append(&request, limits))
        })
        .await
        .map_err(AppendExecutionError::Executor)?
        .map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                AppendExecutionError::CompactionRequired
            } else {
                AppendExecutionError::Storage(error)
            }
        })
    }

    /// Apply at most 64 committed entries under the same single-flight owner.
    /// # Errors
    /// Any decode or persistence failure stops the owner; replies follow commit.
    pub async fn apply_committed(&self) -> Result<Vec<Vec<u8>>, AppendExecutionError> {
        self.run(|gate| Ok(gate.store.apply_committed()))
            .await
            .map_err(AppendExecutionError::Executor)?
            .map_err(AppendExecutionError::Storage)
    }

    /// Publish a coherent durable snapshot without compacting the retained log.
    /// # Errors
    /// Storage or encoding failure stops this owner before returning a snapshot.
    pub async fn publish_snapshot(&self) -> Result<Vec<u8>, AppendExecutionError> {
        self.run(|gate| Ok(gate.store.publish_snapshot()))
            .await
            .map_err(AppendExecutionError::Executor)?
            .map_err(AppendExecutionError::Storage)
    }

    /// Compact through the latest published snapshot without installing remote state.
    /// # Errors
    /// Failure stops this owner; retained entries are deleted only with the boundary commit.
    pub async fn compact_snapshot(&self) -> Result<(), AppendExecutionError> {
        self.run(|gate| Ok(gate.store.compact_snapshot()))
            .await
            .map_err(AppendExecutionError::Executor)?
            .map_err(AppendExecutionError::Storage)
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut DurableVoteGate) -> Result<T, VoteGateError> + Send + 'static,
    ) -> Result<T, VoteGateError> {
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(VoteGateError::Stopped);
        }
        let mut guard = self
            .gate
            .clone()
            .try_lock_owned()
            .map_err(|_| VoteGateError::Busy)?;
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(VoteGateError::Stopped);
        }
        tokio::task::spawn_blocking(move || {
            let mut gate = guard.take().ok_or(VoteGateError::Stopped)?;
            let response = operation(&mut gate);
            *guard = Some(gate);
            drop(guard);
            response
        })
        .await
        .map_err(|_| VoteGateError::Stopped)?
    }

    #[cfg(test)]
    async fn request_with_context(
        &self,
        local: NodeId,
        last_log: Option<LogIdentity>,
        membership: Membership,
        request: RequestVote,
    ) -> Result<VoteResponse, VoteGateError> {
        self.run(move |gate| gate.request(local, last_log, &membership, request))
            .await
    }

    /// Close admission, wait for admitted work, and release store ownership.
    ///
    /// Once polled, cancellation never reopens admission. A cancelled caller
    /// must retry shutdown to wait for release; Drop alone does not report it.
    /// # Errors
    /// Reports blocking-task failure rather than claiming successful shutdown.
    pub async fn shutdown(&self) -> Result<(), VoteGateError> {
        self.closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut guard = self.gate.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            if let Some(gate) = guard.as_mut()
                && let Some((_, staging)) = gate.staging.as_mut()
            {
                staging.clear().map_err(|_| VoteGateError::StorageFailure)?;
            }
            guard.take().map_or(Err(VoteGateError::Stopped), |gate| {
                gate.store
                    .close()
                    .map_err(|_| VoteGateError::StorageFailure)
            })
        })
        .await
        .map_err(|_| VoteGateError::Stopped)?
    }
}

#[cfg(test)]
fn validate_staging_directory(directory: &Path) -> io::Result<()> {
    crate::transfer_staging::open_directory(directory).map(|_| ())
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

impl OwnedVoteStore {
    /// Explicit fresh initialization; an existing path is never truncated.
    /// # Errors
    /// Failed or interrupted initialization remains an explicit error on open.
    pub fn create(path: &Path, cluster: &str) -> io::Result<Self> {
        Self::initialize(path, cluster, None)
    }

    /// Explicitly establish a fresh single-founder cluster. The authenticated
    /// provisioning layer owns cluster identity and authorization; this never
    /// adopts an existing store or bypasses subsequent membership transitions.
    /// # Errors
    /// Rejects joint/multi-voter bootstrap and any existing canonical path.
    pub fn bootstrap(
        path: &Path,
        cluster: &str,
        founder: NodeId,
        membership: Membership,
    ) -> io::Result<Self> {
        if membership.configurations().len() != 1
            || membership.configurations()[0].len() != 1
            || !membership.configurations()[0].contains(&founder)
        {
            return Err(invalid("bootstrap requires one founding voter"));
        }
        let entry = Entry {
            identity: LogIdentity {
                term: Term(0),
                leader: founder,
                index: crate::consensus_model::LogIndex(0),
            },
            payload: EntryPayload::Membership(membership),
        };
        Self::initialize(path, cluster, Some(entry))
    }

    fn initialize(path: &Path, cluster: &str, genesis: Option<Entry>) -> io::Result<Self> {
        let genesis = genesis
            .map(|entry| {
                crate::consensus_storage::encode_owned_entry(&entry)
                    .map(|bytes| (entry.identity, bytes))
                    .map_err(io::Error::other)
            })
            .transpose()?;
        if genesis
            .as_ref()
            .is_some_and(|(_, bytes)| bytes.len() > 1024 * 1024)
        {
            return Err(invalid("bootstrap membership exceeds size limit"));
        }
        if cluster.is_empty() || cluster.len() > 1024 {
            return Err(invalid("invalid cluster identity"));
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // Never expose a partially initialized database at the authority path.
        // persist_noclobber publishes the committed inode without replacing any
        // existing authority; concurrent initializers have exactly one winner.
        let temporary = tempfile::Builder::new()
            .prefix(".owned-vote-init-")
            .tempfile_in(parent)?;
        let backend = OwnedBackend::initialize(temporary.as_file().try_clone()?, cluster)?;
        let status = backend.status.clone();
        let database = Database::builder()
            .create_with_backend(backend)
            .map_err(io::Error::other)?;
        let state = HardState {
            term: Term(0),
            voted_for: None,
        };
        let mut transaction = database.begin_write().map_err(io::Error::other)?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        {
            let mut table = transaction.open_table(TABLE).map_err(io::Error::other)?;
            table
                .insert(
                    "control",
                    crate::control_state::ControlState::new(cluster)
                        .encode_snapshot()
                        .map_err(io::Error::other)?
                        .as_slice(),
                )
                .map_err(io::Error::other)?;
            table.insert("format", FORMAT).map_err(io::Error::other)?;
            for key in EMPTY_POSITIONS {
                table.insert(key, &[][..]).map_err(io::Error::other)?;
            }
            table
                .insert("cluster", cluster.as_bytes())
                .map_err(io::Error::other)?;
            table
                .insert("vote", encode(state)?.as_slice())
                .map_err(io::Error::other)?;
        }
        transaction.open_table(LOG).map_err(io::Error::other)?;
        transaction
            .open_table(MEMBERSHIPS)
            .map_err(io::Error::other)?;
        if let Some((identity, bytes)) = genesis {
            transaction
                .open_table(LOG)
                .map_err(io::Error::other)?
                .insert(identity.index.0, bytes.as_slice())
                .map_err(io::Error::other)?;
            transaction
                .open_table(MEMBERSHIPS)
                .map_err(io::Error::other)?
                .insert(identity.index.0, bytes.as_slice())
                .map_err(io::Error::other)?;
            let mut meta = transaction.open_table(TABLE).map_err(io::Error::other)?;
            meta.insert("membership", bytes.as_slice())
                .map_err(io::Error::other)?;
            meta.insert("commit", encode_position(Some(identity)).as_slice())
                .map_err(io::Error::other)?;
        }
        transaction.commit().map_err(io::Error::other)?;
        // The database retains its exclusive lock throughout publication.
        #[cfg(test)]
        initialization_crash("before-publish");
        let published = temporary
            .persist_noclobber(path)
            .map_err(io::Error::other)?;
        #[cfg(test)]
        initialization_crash("after-publish");
        published.sync_all()?;
        File::open(parent)?.sync_all()?;
        Ok(Self {
            database,
            status,
            state,
            failed: false,
        })
    }

    /// Recover a committed owned vote with an exact cluster binding.
    /// # Errors
    /// Missing metadata, unknown formats, wrong clusters and corrupt votes fail.
    /// redb holds exclusive database ownership for this handle's lifetime.
    pub fn open(path: &Path, cluster: &str) -> io::Result<Self> {
        // Validate identification while holding the same inode lock that redb
        // retains through recovery. Unsupported files never reach writable redb.
        let backend = OwnedBackend::open(path, cluster)?;
        preflight_payload(&backend, cluster)?;
        // A previous initializer may have published this inode but failed before
        // directory sync. Complete that durability boundary before exposing it.
        backend.operate(|file| file.sync_all())?;
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?
        .sync_all()?;
        let status = backend.status.clone();
        let database = Database::builder()
            .create_with_backend(backend)
            .map_err(io::Error::other)?;
        let state = read_vote(&database, cluster)?;
        Ok(Self {
            database,
            status,
            state,
            failed: false,
        })
    }

    /// Release storage and verify the backend completed cleanup without errors.
    /// # Errors
    /// Missing close completion or any observed backend failure rejects success.
    pub fn close(self) -> io::Result<()> {
        let Self {
            database,
            status,
            failed,
            ..
        } = self;
        drop(database);
        if failed || status.load(std::sync::atomic::Ordering::SeqCst) != 1 {
            return Err(io::Error::other(
                "owned database cleanup failed or incomplete",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn state(&self) -> HardState {
        self.state
    }

    fn compact_snapshot(&mut self) -> io::Result<()> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let mut tx = self.database.begin_write().map_err(io::Error::other)?;
        tx.set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        {
            let mut meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let bytes = meta
                .get("published_snapshot")
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("no published snapshot"))?
                .value()
                .to_vec();
            let (descriptor, _) = crate::consensus_storage::decode_owned_snapshot(&bytes)
                .map_err(io::Error::other)?;
            let boundary = descriptor
                .last_included
                .ok_or_else(|| invalid("empty snapshot"))?;
            meta.insert("compacted_snapshot", bytes.as_slice())
                .map_err(io::Error::other)?;
            meta.insert("snapshot", encode_position(Some(boundary)).as_slice())
                .map_err(io::Error::other)?;
            compact_table(&tx, LOG, boundary.index.0)?;
            compact_table(&tx, MEMBERSHIPS, boundary.index.0)?;
        }
        tx.commit().map_err(io::Error::other)?;
        self.failed = false;
        Ok(())
    }

    fn publish_snapshot(&mut self) -> io::Result<Vec<u8>> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let mut tx = self.database.begin_write().map_err(io::Error::other)?;
        tx.set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        let bytes = {
            let mut meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let get = |key| -> io::Result<Vec<u8>> {
                Ok(meta
                    .get(key)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing snapshot source"))?
                    .value()
                    .to_vec())
            };
            let applied = decode_position(&get("applied")?)?
                .ok_or_else(|| invalid("snapshot requires applied state"))?;
            let entry = crate::consensus_storage::decode_owned_entry(&get("applied_membership")?)
                .map_err(io::Error::other)?;
            let EntryPayload::Membership(membership) = entry.payload else {
                return Err(invalid("snapshot requires applied membership"));
            };
            let membership = crate::consensus_model::StoredMembership {
                introduced_at: Some(entry.identity),
                membership,
            };
            let cluster = String::from_utf8(get("cluster")?).map_err(io::Error::other)?;
            let bytes = crate::consensus_storage::encode_owned_snapshot(
                &cluster,
                &uuid::Uuid::new_v4().to_string(),
                applied,
                &membership,
                &get("control")?,
            )
            .map_err(io::Error::other)?;
            // Separate from the compaction boundary: the full log remains authoritative.
            meta.insert("published_snapshot", bytes.as_slice())
                .map_err(io::Error::other)?;
            bytes
        };
        tx.commit().map_err(io::Error::other)?;
        self.failed = false;
        Ok(bytes)
    }

    fn apply_committed(&mut self) -> io::Result<Vec<Vec<u8>>> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let mut tx = self.database.begin_write().map_err(io::Error::other)?;
        tx.set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        let mut replies = Vec::new();
        {
            let mut meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let get = |key| -> io::Result<Vec<u8>> {
                Ok(meta
                    .get(key)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing application metadata"))?
                    .value()
                    .to_vec())
            };
            let committed = decode_position(&get("commit")?)?;
            let mut applied = decode_position(&get("applied")?)?;
            let mut membership_bytes = get("applied_membership")?;
            let mut state = crate::control_state::ControlState::decode_snapshot(&get("control")?)
                .map_err(io::Error::other)?;
            let log = tx.open_table(LOG).map_err(io::Error::other)?;
            let start = applied.map_or(Some(0), |position| position.index.0.checked_add(1));
            if let (Some(start), Some(committed)) = (start, committed) {
                let mut budget = 1024 * 1024usize;
                for record in log
                    .range(start..=committed.index.0)
                    .map_err(io::Error::other)?
                    .take(64)
                {
                    let (_, bytes) = record.map_err(io::Error::other)?;
                    if bytes.value().len() > budget {
                        if replies.is_empty() {
                            return Err(invalid("committed entry exceeds application budget"));
                        }
                        break;
                    }
                    budget -= bytes.value().len();
                    let entry = crate::consensus_storage::decode_owned_entry(bytes.value())
                        .map_err(io::Error::other)?;
                    let reply = match entry.payload {
                        EntryPayload::NoOp => Vec::new(),
                        EntryPayload::Membership(_) => {
                            membership_bytes = bytes.value().to_vec();
                            Vec::new()
                        }
                        EntryPayload::Application(command) => {
                            let membership =
                                crate::consensus_storage::decode_owned_entry(&membership_bytes)
                                    .map_err(io::Error::other)?;
                            let EntryPayload::Membership(configuration) = membership.payload else {
                                return Err(invalid("missing applied membership"));
                            };
                            let membership = crate::consensus_model::StoredMembership {
                                introduced_at: Some(membership.identity),
                                membership: configuration,
                            };
                            crate::consensus_storage::apply_control_command(
                                &mut state,
                                crate::consensus_storage::ApplicationMembership::Owned(&membership),
                                &command,
                            )?
                        }
                    };
                    applied = Some(entry.identity);
                    replies.push(reply);
                }
            }
            let bytes = state.encode_snapshot().map_err(io::Error::other)?;
            meta.insert("control", bytes.as_slice())
                .map_err(io::Error::other)?;
            meta.insert("applied", encode_position(applied).as_slice())
                .map_err(io::Error::other)?;
            meta.insert("applied_membership", membership_bytes.as_slice())
                .map_err(io::Error::other)?;
        }
        #[cfg(test)]
        application_crash("before-commit");
        tx.commit().map_err(io::Error::other)?;
        #[cfg(test)]
        application_crash("after-commit");
        self.failed = false;
        Ok(replies)
    }

    fn vote(&mut self, local: NodeId, request: RequestVote) -> Result<VoteResponse, VoteGateError> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(VoteGateError::Stopped);
        }
        self.failed = true;
        let context = (|| -> io::Result<_> {
            let transaction = self.database.begin_read().map_err(io::Error::other)?;
            let log = transaction.open_table(LOG).map_err(io::Error::other)?;
            let last_log = log
                .last()
                .map_err(io::Error::other)?
                .map(|(_, bytes)| {
                    crate::consensus_storage::decode_owned_entry(bytes.value())
                        .map(|entry| entry.identity)
                        .map_err(io::Error::other)
                })
                .transpose()?;
            let meta = transaction.open_table(TABLE).map_err(io::Error::other)?;
            let bytes = meta
                .get("membership")
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("missing membership"))?;
            let membership = if bytes.value().is_empty() {
                None
            } else {
                match crate::consensus_storage::decode_owned_entry(bytes.value())
                    .map_err(io::Error::other)?
                    .payload
                {
                    EntryPayload::Membership(membership) => Some(membership),
                    _ => return Err(invalid("invalid membership authority")),
                }
            };
            Ok((
                last_log.or(decode_position(
                    meta.get("snapshot")
                        .map_err(io::Error::other)?
                        .ok_or_else(|| invalid("missing snapshot"))?
                        .value(),
                )?),
                membership,
            ))
        })()
        .map_err(|_| VoteGateError::StorageFailure)?;
        self.failed = false;
        let Some(membership) = context.1 else {
            // An uninitialized node cannot vote; bootstrap must establish durable
            // membership explicitly before election admission.
            if request.term > self.state.term {
                self.persist(HardState {
                    term: request.term,
                    voted_for: None,
                })
                .map_err(|_| VoteGateError::StorageFailure)?;
            }
            return Ok(VoteResponse {
                term: self.state.term,
                granted: false,
            });
        };
        let decision = self
            .state
            .evaluate_vote(local, context.0, &membership, request);
        if let Some(state) = decision.persist {
            self.persist(state)
                .map_err(|_| VoteGateError::StorageFailure)?;
        }
        Ok(decision.response)
    }

    /// Persist an authenticated append under the same owner as votes. The caller
    /// must serialize this blocking operation and authenticate the peer first.
    /// # Errors
    /// Storage failure permanently stops admission; no decision is returned until
    /// all dependent writes have committed with immediate durability.
    pub fn append(
        &mut self,
        request: &AppendRequest,
        limits: (usize, usize),
    ) -> io::Result<AppendDecision> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        validate_append_admission(request, limits)?;
        self.failed = true;
        let mut transaction = self.database.begin_write().map_err(io::Error::other)?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        let decision;
        {
            let mut meta = transaction.open_table(TABLE).map_err(io::Error::other)?;
            let committed = decode_position(
                meta.get("commit")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing commit"))?
                    .value(),
            )?;
            let mut log = transaction.open_table(LOG).map_err(io::Error::other)?;
            let mut memberships = transaction
                .open_table(MEMBERSHIPS)
                .map_err(io::Error::other)?;
            let mut entries = std::collections::BTreeMap::<u64, Entry>::new();
            for identity in request
                .entries
                .iter()
                .map(|entry| entry.identity)
                .chain(request.previous)
                .chain(committed)
                .chain(request.leader_commit)
            {
                if let Some(bytes) = log.get(identity.index.0).map_err(io::Error::other)? {
                    entries.insert(
                        identity.index.0,
                        crate::consensus_storage::decode_owned_entry(bytes.value())
                            .map_err(io::Error::other)?,
                    );
                }
            }
            let snapshot = write_position(&meta, "snapshot")?;
            decision = self.state.evaluate_append(
                request,
                (committed, snapshot),
                |index| Ok(entries.get(&index.0)),
                limits,
            );
            if let Some(state) = decision.persist {
                meta.insert("vote", encode(state)?.as_slice())
                    .map_err(io::Error::other)?;
            }
            if let Ok(AppendOutcome::Reconciled(AppendReconciliation::Matched {
                replace_from,
                ..
            })) = decision.outcome
            {
                if let Some(offset) = replace_from {
                    let first = request.entries[offset].identity.index.0;
                    if retained_after_replace(&log, first, request.entries.len() - offset)?
                        > MAX_RETAINED_ENTRIES
                    {
                        self.failed = false;
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "owned log requires compaction",
                        ));
                    }
                    log.retain_in(first.., |_, _| false)
                        .map_err(io::Error::other)?;
                    memberships
                        .retain_in(first.., |_, _| false)
                        .map_err(io::Error::other)?;
                    for entry in &request.entries[offset..] {
                        let bytes = crate::consensus_storage::encode_owned_entry(entry)
                            .map_err(io::Error::other)?;
                        log.insert(entry.identity.index.0, bytes.as_slice())
                            .map_err(io::Error::other)?;
                        if matches!(entry.payload, EntryPayload::Membership(_)) {
                            memberships
                                .insert(entry.identity.index.0, bytes.as_slice())
                                .map_err(io::Error::other)?;
                        }
                    }
                    let fallback = application_membership_bytes(&meta)?;
                    let latest = memberships
                        .last()
                        .map_err(io::Error::other)?
                        .map_or(fallback, |(_, bytes)| bytes.value().to_vec());
                    meta.insert("membership", latest.as_slice())
                        .map_err(io::Error::other)?;
                }
                meta.insert("commit", encode_position(decision.commit).as_slice())
                    .map_err(io::Error::other)?;
            }
        }
        transaction.commit().map_err(io::Error::other)?;
        if let Some(state) = decision.persist {
            self.state = state;
        }
        self.failed = false;
        Ok(decision)
    }

    /// Atomically replace authority, acknowledging only an immediate commit.
    /// # Errors
    /// Rejects regression; transaction failures permanently stop the handle.
    pub fn persist(&mut self, state: HardState) -> io::Result<()> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) == 2 {
            return Err(io::Error::other("owned vote store stopped"));
        }
        if state.term < self.state.term
            || (state.term == self.state.term
                && self.state.voted_for.is_some()
                && state.voted_for != self.state.voted_for)
        {
            return Err(invalid("vote authority regression"));
        }
        let bytes = encode(state)?;
        self.failed = true;
        let mut transaction = self.database.begin_write().map_err(io::Error::other)?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        transaction
            .open_table(TABLE)
            .map_err(io::Error::other)?
            .insert("vote", bytes.as_slice())
            .map_err(io::Error::other)?;
        transaction.commit().map_err(io::Error::other)?;
        self.state = state;
        self.failed = false;
        Ok(())
    }
}

fn encode(state: HardState) -> io::Result<[u8; 41]> {
    let mut bytes = [0; 41];
    bytes[..8].copy_from_slice(&state.term.0.to_le_bytes());
    if let Some(id) = state.voted_for {
        id.public_key().map_err(io::Error::other)?;
        bytes[8] = 1;
        bytes[9..].copy_from_slice(id.as_bytes());
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> io::Result<HardState> {
    if bytes.len() != 41 || bytes[8] > 1 {
        return Err(invalid("corrupt vote"));
    }
    let term = Term(u64::from_le_bytes(
        bytes[..8].try_into().map_err(io::Error::other)?,
    ));
    let voted_for = if bytes[8] == 1 {
        let id = NodeId::from_bytes(bytes[9..].try_into().map_err(io::Error::other)?);
        id.public_key().map_err(io::Error::other)?;
        Some(id)
    } else {
        if bytes[9..].iter().any(|b| *b != 0) {
            return Err(invalid("corrupt absent vote"));
        }
        None
    };
    Ok(HardState { term, voted_for })
}

#[cfg(test)]
fn application_crash(point: &str) {
    if std::env::var("BMUX_OWNED_APPLY_CRASH").as_deref() == Ok(point) {
        std::process::exit(74);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn staging_directory_rejects_leftovers_without_deleting_them() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let directory = tempfile::tempdir_in(parent).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        validate_staging_directory(directory.path()).unwrap();
        let leftover = directory.path().join("unknown");
        std::fs::write(&leftover, b"preserve").unwrap();
        assert!(validate_staging_directory(directory.path()).is_err());
        assert_eq!(std::fs::read(leftover).unwrap(), b"preserve");
    }

    #[tokio::test]
    async fn staging_expires_without_transfer_requests() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let root = tempfile::tempdir_in(parent).unwrap();
        let directory = root.path().join("staging");
        std::fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = OwnedVoteStore::create(&root.path().join("owned"), "cluster").unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        executor
            .configure_staging(directory.clone(), 16, 32)
            .await
            .unwrap();
        let key = crate::transfer_staging::TransferKey {
            peer: [1; 32],
            incarnation: uuid::Uuid::nil(),
            sequence: 1,
        };
        executor
            .run(move |gate| {
                let (directory, staging) = gate.staging.as_mut().unwrap();
                staging
                    .begin(
                        directory,
                        key,
                        [0; 32],
                        4,
                        [0; 32],
                        std::time::Instant::now()
                            .checked_sub(std::time::Duration::from_secs(31))
                            .unwrap(),
                    )
                    .unwrap();
                Ok(())
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if std::fs::read_dir(&directory).unwrap().count() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        executor.shutdown().await.unwrap();
    }
    use super::*;

    #[test]
    fn application_crash_child() {
        let Some(path) = std::env::var_os("BMUX_OWNED_APPLY_PATH") else {
            return;
        };
        OwnedVoteStore::open(Path::new(&path), "cluster")
            .unwrap()
            .apply_committed()
            .unwrap();
        panic!("application crash hook not reached");
    }

    fn crash_application(path: &Path, point: &str) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "owned_vote_store::tests::application_crash_child",
                "--nocapture",
            ])
            .env("BMUX_OWNED_APPLY_PATH", path)
            .env("BMUX_OWNED_APPLY_CRASH", point)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(74));
    }

    fn assert_snapshot(store: &mut OwnedVoteStore, position: LogIdentity) {
        let snapshot = store.publish_snapshot().unwrap();
        let (descriptor, control) =
            crate::consensus_storage::decode_owned_snapshot(&snapshot).unwrap();
        assert_eq!(descriptor.last_included, Some(position));
        let restored = crate::control_state::ControlState::decode_snapshot(&control).unwrap();
        assert_eq!(restored.revision, 1);
    }

    #[test]
    fn application_crash_recovers_mutation_and_duplicate_together() {
        use bmux_cluster_plugin_api::cluster_types::{
            CommandId, ControlCommand, ControlCommandRequest, WorkspaceId,
        };
        for point in ["before-commit", "after-commit"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("authority.redb");
            let node = NodeId::from(1);
            let identity = |index| LogIdentity {
                term: Term(1),
                leader: node,
                index: crate::consensus_model::LogIndex(index),
            };
            let command = ControlCommand {
                schema_version: 1,
                principal_id: "principal:test".into(),
                command_id: CommandId {
                    value: uuid::Uuid::from_u128(10),
                },
                issued_at_unix_ms: 50,
                request: ControlCommandRequest::CreateWorkspace {
                    workspace_id: WorkspaceId {
                        value: uuid::Uuid::from_u128(20),
                    },
                    name: Some("workspace".into()),
                },
            };
            let bytes = crate::control_codec::encode_control_command(&command);
            let mut store = OwnedVoteStore::bootstrap(
                &path,
                "cluster",
                node,
                Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
            )
            .unwrap();
            store.apply_committed().unwrap();
            store
                .append(
                    &AppendRequest {
                        term: Term(1),
                        leader: node,
                        previous: Some(LogIdentity {
                            term: Term(0),
                            ..identity(0)
                        }),
                        entries: vec![
                            Entry {
                                identity: identity(1),
                                payload: EntryPayload::Application(bytes.clone()),
                            },
                            Entry {
                                identity: identity(2),
                                payload: EntryPayload::Application(bytes),
                            },
                        ],
                        leader_commit: Some(identity(2)),
                    },
                    (16, 4096),
                )
                .unwrap();
            store.close().unwrap();
            crash_application(&path, point);
            let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
            {
                let tx = store.database.begin_read().unwrap();
                let meta = tx.open_table(TABLE).unwrap();
                let applied = decode_position(meta.get("applied").unwrap().unwrap().value())
                    .unwrap()
                    .unwrap();
                let state = crate::control_state::ControlState::decode_snapshot(
                    meta.get("control").unwrap().unwrap().value(),
                )
                .unwrap();
                assert_eq!(
                    applied.index.0,
                    if point == "before-commit" { 0 } else { 2 }
                );
                assert_eq!(state.revision, u64::from(point != "before-commit"));
            }
            let replies = store.apply_committed().unwrap();
            if point == "before-commit" {
                assert_eq!(replies.len(), 2);
                assert_eq!(replies[0], replies[1]);
            } else {
                assert!(replies.is_empty());
            }
            assert_snapshot(&mut store, identity(2));
            let tx = store.database.begin_read().unwrap();
            let meta = tx.open_table(TABLE).unwrap();
            let mut state = crate::control_state::ControlState::decode_snapshot(
                meta.get("control").unwrap().unwrap().value(),
            )
            .unwrap();
            let response = state.apply(&command);
            assert_eq!(response.control_revision, 1);
            assert_eq!(state.revision, 1);
        }
    }

    #[tokio::test]
    async fn committed_application_recovers_position_and_membership() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let node = NodeId::from(1);
        let store = OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        assert_eq!(
            executor.apply_committed().await.unwrap(),
            vec![Vec::<u8>::new()]
        );
        assert!(executor.apply_committed().await.unwrap().is_empty());
        let published = executor.publish_snapshot().await.unwrap();
        executor.compact_snapshot().await.unwrap();
        executor.shutdown().await.unwrap();
        let store = OwnedVoteStore::open(&path, "cluster").unwrap();
        let tx = store.database.begin_read().unwrap();
        let meta = tx.open_table(TABLE).unwrap();
        assert_eq!(
            meta.get("published_snapshot").unwrap().unwrap().value(),
            published
        );
        assert_eq!(
            decode_position(meta.get("applied").unwrap().unwrap().value())
                .unwrap()
                .unwrap()
                .index
                .0,
            0
        );
        assert!(
            !meta
                .get("applied_membership")
                .unwrap()
                .unwrap()
                .value()
                .is_empty()
        );
    }

    #[test]
    fn compaction_frees_capacity_beyond_absolute_index_limit() {
        check_compaction_capacity(0);
    }

    #[test]
    fn partial_compaction_preserves_unapplied_suffix_at_capacity() {
        check_compaction_capacity(16);
    }

    fn check_compaction_capacity(retained: u64) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("capacity.redb");
        let node = NodeId::from(1);
        let mut store = OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let identity = |index| LogIdentity {
            term: Term(1),
            leader: node,
            index: crate::consensus_model::LogIndex(index),
        };
        let entries = (1..MAX_RETAINED_ENTRIES)
            .map(|index| Entry {
                identity: identity(index),
                payload: EntryPayload::NoOp,
            })
            .collect();
        store
            .append(
                &AppendRequest {
                    term: Term(1),
                    leader: node,
                    previous: Some(LogIdentity {
                        term: Term(0),
                        ..identity(0)
                    }),
                    entries,
                    leader_commit: Some(identity(MAX_RETAINED_ENTRIES - 1 - retained)),
                },
                (4096, 4096),
            )
            .unwrap();
        let request = AppendRequest {
            term: Term(1),
            leader: node,
            previous: Some(identity(MAX_RETAINED_ENTRIES - 1)),
            entries: vec![Entry {
                identity: identity(MAX_RETAINED_ENTRIES),
                payload: EntryPayload::NoOp,
            }],
            leader_commit: Some(identity(MAX_RETAINED_ENTRIES)),
        };
        assert_eq!(
            store.append(&request, (1, 4096)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        while !store.apply_committed().unwrap().is_empty() {}
        store.publish_snapshot().unwrap();
        store.compact_snapshot().unwrap();
        assert!(store.append(&request, (1, 4096)).unwrap().outcome.is_ok());
        store.close().unwrap();
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(
            u64::try_from(store.apply_committed().unwrap().len()).unwrap(),
            retained + 1
        );
    }

    #[tokio::test]
    async fn append_admission_errors_do_not_poison_authority() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let node = NodeId::from(1);
        let executor = VoteExecutor::new(DurableVoteGate::new(
            1,
            OwnedVoteStore::create(&path, "cluster").unwrap(),
        ));
        let mut request = AppendRequest {
            term: Term(1),
            leader: node,
            previous: None,
            entries: vec![Entry {
                identity: LogIdentity {
                    term: Term(1),
                    leader: node,
                    index: crate::consensus_model::LogIndex(0),
                },
                payload: EntryPayload::NoOp,
            }],
            leader_commit: None,
        };
        assert!(matches!(
            executor.append(request.clone(), (0, 4096)).await,
            Err(AppendExecutionError::InvalidRequest(_))
        ));
        request.previous = Some(LogIdentity {
            term: Term(1),
            leader: node,
            index: crate::consensus_model::LogIndex(MAX_RETAINED_ENTRIES - 1),
        });
        request.entries[0].identity.index.0 = MAX_RETAINED_ENTRIES;
        // A high absolute index is a reconciliation issue, not exhausted storage.
        assert!(executor.append(request.clone(), (1, 4096)).await.is_ok());
        request.previous = None;
        request.entries[0].identity.index.0 = 0;
        let guard = executor.gate.lock().await;
        assert!(matches!(
            executor.append(request.clone(), (1, 4096)).await,
            Err(AppendExecutionError::Executor(VoteGateError::Busy))
        ));
        drop(guard);
        executor.append(request.clone(), (1, 4096)).await.unwrap();
        executor.shutdown().await.unwrap();
        assert!(matches!(
            executor.append(request, (1, 4096)).await,
            Err(AppendExecutionError::Executor(VoteGateError::Stopped))
        ));
        assert_eq!(
            OwnedVoteStore::open(&path, "cluster").unwrap().state().term,
            Term(1)
        );
    }

    #[tokio::test]
    async fn bootstrap_recovers_voting_authority_without_reinitialization() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let founder = NodeId::from(1);
        let membership =
            Membership::new(vec![vec![founder]], vec![(founder, String::new())]).unwrap();
        OwnedVoteStore::bootstrap(&path, "cluster", founder, membership.clone())
            .unwrap()
            .close()
            .unwrap();
        assert!(OwnedVoteStore::bootstrap(&path, "cluster", founder, membership).is_err());
        let executor = VoteExecutor::recover(path.clone(), "cluster".into(), 1)
            .await
            .unwrap();
        let identity = LogIdentity {
            term: Term(0),
            leader: founder,
            index: crate::consensus_model::LogIndex(0),
        };
        assert!(
            executor
                .request(
                    founder,
                    RequestVote {
                        term: Term(1),
                        candidate: founder,
                        last_log: Some(identity)
                    }
                )
                .await
                .unwrap()
                .granted
        );
        executor.shutdown().await.unwrap();
        let store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(store.state().voted_for, Some(founder));
        store.close().unwrap();
    }

    #[tokio::test]
    async fn executor_votes_from_appended_authority_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let node = NodeId::from(1);
        let identity = LogIdentity {
            term: Term(1),
            leader: node,
            index: crate::consensus_model::LogIndex(0),
        };
        let executor = VoteExecutor::new(DurableVoteGate::new(
            1,
            OwnedVoteStore::create(&path, "cluster").unwrap(),
        ));
        executor
            .append(
                AppendRequest {
                    term: Term(1),
                    leader: node,
                    previous: None,
                    entries: vec![Entry {
                        identity,
                        payload: EntryPayload::Membership(
                            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
                        ),
                    }],
                    leader_commit: Some(identity),
                },
                (16, 4096),
            )
            .await
            .unwrap();
        let stale = RequestVote {
            term: Term(2),
            candidate: node,
            last_log: None,
        };
        assert!(!executor.request(node, stale).await.unwrap().granted);
        executor.shutdown().await.unwrap();
        let executor = VoteExecutor::recover(path.clone(), "cluster".into(), 2)
            .await
            .unwrap();
        assert!(
            executor
                .request(
                    node,
                    RequestVote {
                        last_log: Some(identity),
                        ..stale
                    }
                )
                .await
                .unwrap()
                .granted
        );
        executor.shutdown().await.unwrap();
        assert_eq!(
            OwnedVoteStore::open(&path, "cluster")
                .unwrap()
                .state()
                .voted_for,
            Some(node)
        );
    }

    #[test]
    fn append_commits_authority_and_repairs_membership_on_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let leader = NodeId::from_bytes([1; 32]);
        let identity = |term, index| LogIdentity {
            term: Term(term),
            leader,
            index: crate::consensus_model::LogIndex(index),
        };
        let membership =
            Membership::new(vec![vec![leader]], vec![(leader, "endpoint".to_owned())]).unwrap();
        let mut store = OwnedVoteStore::create(&path, "cluster").unwrap();
        let initial = AppendRequest {
            term: Term(1),
            leader,
            previous: None,
            entries: vec![
                Entry {
                    identity: identity(1, 0),
                    payload: EntryPayload::NoOp,
                },
                Entry {
                    identity: identity(1, 1),
                    payload: EntryPayload::Membership(membership),
                },
            ],
            leader_commit: Some(identity(1, 0)),
        };
        assert!(store.append(&initial, (16, 4096)).unwrap().outcome.is_ok());
        store.close().unwrap();
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        let replacement = AppendRequest {
            term: Term(2),
            leader,
            previous: Some(identity(1, 0)),
            entries: vec![Entry {
                identity: identity(2, 1),
                payload: EntryPayload::NoOp,
            }],
            leader_commit: Some(identity(2, 1)),
        };
        let decision = store.append(&replacement, (16, 4096)).unwrap();
        assert_eq!(decision.commit, Some(identity(2, 1)));
        store.close().unwrap();
        let store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(store.state().term, Term(2));
        let transaction = store.database.begin_read().unwrap();
        assert_eq!(
            transaction.open_table(MEMBERSHIPS).unwrap().len().unwrap(),
            0
        );
        let meta = transaction.open_table(TABLE).unwrap();
        assert_eq!(
            decode_position(meta.get("commit").unwrap().unwrap().value()).unwrap(),
            Some(identity(2, 1))
        );
        assert!(meta.get("membership").unwrap().unwrap().value().is_empty());
    }
    #[test]
    fn committed_vote_survives_aborted_update_and_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let mut store = OwnedVoteStore::create(&path, "cluster-a").unwrap();
        assert!(OwnedVoteStore::create(&path, "cluster-a").is_err());
        assert!(OwnedVoteStore::open(&path, "cluster-a").is_err());
        let state = HardState {
            term: Term(2),
            voted_for: Some(NodeId::from(1)),
        };
        store.persist(state).unwrap();
        {
            let transaction = store.database.begin_write().unwrap();
            transaction
                .open_table(TABLE)
                .unwrap()
                .insert(
                    "vote",
                    encode(HardState {
                        term: Term(3),
                        voted_for: None,
                    })
                    .unwrap()
                    .as_slice(),
                )
                .unwrap();
            transaction.abort().unwrap();
        }
        drop(store);
        assert!(OwnedVoteStore::open(&path, "cluster-b").is_err());
        let mut reopened = OwnedVoteStore::open(&path, "cluster-a").unwrap();
        assert_eq!(reopened.state(), state);
        assert!(
            reopened
                .persist(HardState {
                    term: Term(2),
                    voted_for: None
                })
                .is_err()
        );
    }

    #[test]
    fn durable_gate_recovers_vote_and_stops_on_storage_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let node = NodeId::from(1);
        let other = NodeId::from(2);
        let membership = Membership::new(
            vec![vec![node, other]],
            vec![(node, String::new()), (other, String::new())],
        )
        .unwrap();
        let request = RequestVote {
            term: Term(2),
            candidate: node,
            last_log: None,
        };
        let mut gate = DurableVoteGate::new(1, OwnedVoteStore::create(&path, "cluster-a").unwrap());
        assert!(
            gate.request(node, None, &membership, request)
                .unwrap()
                .granted
        );
        drop(gate);
        let mut gate = DurableVoteGate::new(2, OwnedVoteStore::open(&path, "cluster-a").unwrap());
        assert!(
            !gate
                .request(
                    node,
                    None,
                    &membership,
                    RequestVote {
                        candidate: other,
                        ..request
                    }
                )
                .unwrap()
                .granted
        );
        gate.store.failed = true;
        assert_eq!(
            gate.request(
                node,
                None,
                &membership,
                RequestVote {
                    term: Term(3),
                    ..request
                }
            ),
            Err(VoteGateError::StorageFailure)
        );
        assert_eq!(
            gate.request(node, None, &membership, request),
            Err(VoteGateError::Stopped)
        );
        drop(gate);
        assert_eq!(
            OwnedVoteStore::open(&path, "cluster-a")
                .unwrap()
                .state()
                .term,
            Term(2)
        );
    }

    // Exits without Rust destructors to qualify recovery with an unclean redb
    // shutdown, both after a vote reply and during an uncommitted replacement.
    #[test]
    fn crash_child() {
        let Some(path) = std::env::var_os("BMUX_OWNED_VOTE_CRASH_PATH") else {
            return;
        };
        let path = Path::new(&path);
        let store = OwnedVoteStore::open(path, "cluster-a").unwrap();
        let node = NodeId::from(1);
        let membership = Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
        let mut gate = DurableVoteGate::new(5, store);
        assert!(
            gate.request(
                node,
                None,
                &membership,
                RequestVote {
                    term: Term(2),
                    candidate: node,
                    last_log: None
                }
            )
            .unwrap()
            .granted
        );
        if std::env::var_os("BMUX_OWNED_VOTE_ABORT").is_some() {
            let transaction = gate.store.database.begin_write().unwrap();
            transaction
                .open_table(TABLE)
                .unwrap()
                .insert(
                    "vote",
                    encode(HardState {
                        term: Term(3),
                        voted_for: None,
                    })
                    .unwrap()
                    .as_slice(),
                )
                .unwrap();
            std::process::exit(71);
        }
        std::process::exit(71);
    }

    #[test]
    fn acknowledged_vote_survives_process_exit() {
        for abort in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("vote.redb");
            drop(OwnedVoteStore::create(&path, "cluster-a").unwrap());
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "owned_vote_store::tests::crash_child",
                    "--nocapture",
                ])
                .env("BMUX_OWNED_VOTE_CRASH_PATH", &path)
                .env_remove("BMUX_OWNED_VOTE_ABORT");
            if abort {
                child.env("BMUX_OWNED_VOTE_ABORT", "1");
            }
            assert_eq!(child.status().unwrap().code(), Some(71));
            assert_eq!(
                OwnedVoteStore::open(&path, "cluster-a").unwrap().state(),
                HardState {
                    term: Term(2),
                    voted_for: Some(NodeId::from(1))
                }
            );
        }
    }

    #[tokio::test]
    async fn executor_bounds_admission_and_releases_store_on_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let executor = VoteExecutor::new(DurableVoteGate::new(
            1,
            OwnedVoteStore::create(&path, "cluster-a").unwrap(),
        ));
        let node = NodeId::from(1);
        let membership = Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
        let request = RequestVote {
            term: Term(2),
            candidate: node,
            last_log: None,
        };
        let guard = executor.gate.lock().await;
        assert_eq!(
            executor
                .request_with_context(node, None, membership.clone(), request)
                .await,
            Err(VoteGateError::Busy)
        );
        drop(guard);
        assert!(
            executor
                .request_with_context(node, None, membership.clone(), request)
                .await
                .unwrap()
                .granted
        );
        executor.shutdown().await.unwrap();
        assert_eq!(
            executor
                .request_with_context(node, None, membership, request)
                .await,
            Err(VoteGateError::Stopped)
        );
        assert_eq!(
            OwnedVoteStore::open(&path, "cluster-a")
                .unwrap()
                .state()
                .term,
            Term(2)
        );
    }

    #[tokio::test]
    async fn cancelled_shutdown_never_reopens_admission() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let executor = VoteExecutor::new(DurableVoteGate::new(
            1,
            OwnedVoteStore::create(&path, "cluster-a").unwrap(),
        ));
        let guard = executor.gate.lock().await;
        let mut shutdown = Box::pin(executor.shutdown());
        // Poll once while ownership is held, then cancel deterministically.
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(shutdown.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(shutdown);
        drop(guard);
        let node = NodeId::from(1);
        let membership = Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
        assert_eq!(
            executor
                .request_with_context(
                    node,
                    None,
                    membership,
                    RequestVote {
                        term: Term(1),
                        candidate: node,
                        last_log: None
                    }
                )
                .await,
            Err(VoteGateError::Stopped)
        );
        assert!(OwnedVoteStore::open(&path, "cluster-a").is_err());
        executor.shutdown().await.unwrap();
        assert_eq!(
            OwnedVoteStore::open(&path, "cluster-a")
                .unwrap()
                .state()
                .term,
            Term(0)
        );
    }

    #[test]
    fn concurrent_initializers_publish_one_authority() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let barrier = std::sync::Barrier::new(2);
        let (a, b) = std::thread::scope(|scope| {
            let initialize = |cluster| {
                barrier.wait();
                OwnedVoteStore::create(&path, cluster)
            };
            let a = scope.spawn(move || initialize("cluster-a"));
            let b = scope.spawn(move || initialize("cluster-b"));
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(a.is_ok(), b.is_ok());
        let cluster = if a.is_ok() { "cluster-a" } else { "cluster-b" };
        drop((a, b));
        assert_eq!(
            OwnedVoteStore::open(&path, cluster).unwrap().state().term,
            Term(0)
        );
    }

    #[test]
    fn preflight_rejects_without_mutating_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        drop(Database::create(&path).unwrap());
        let legacy = std::fs::read(&path).unwrap();
        assert!(OwnedVoteStore::open(&path, "cluster-a").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), legacy);

        let owned = root.path().join("owned.redb");
        drop(OwnedVoteStore::create(&owned, "cluster-a").unwrap());
        let original = std::fs::read(&owned).unwrap();
        assert!(OwnedVoteStore::open(&owned, "cluster-b").is_err());
        assert_eq!(std::fs::read(&owned).unwrap(), original);
        for index in [0, 8, 16, HEADER_BYTES - 1] {
            let mut corrupt = original.clone();
            corrupt[index] ^= 1;
            std::fs::write(&owned, &corrupt).unwrap();
            assert!(OwnedVoteStore::open(&owned, "cluster-a").is_err());
            assert_eq!(std::fs::read(&owned).unwrap(), corrupt);
        }
        assert!(payload_offset(u64::MAX).is_err());
    }

    #[test]
    fn close_reports_sticky_backend_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let store = OwnedVoteStore::create(&path, "cluster-a").unwrap();
        store.status.store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(store.close().is_err());
        OwnedVoteStore::open(&path, "cluster-a")
            .unwrap()
            .close()
            .unwrap();
    }

    #[test]
    fn backend_read_failure_stops_following_io_and_vote_replies() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        OwnedVoteStore::create(&path, "cluster-a")
            .unwrap()
            .close()
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let backend = OwnedBackend::open(&path, "cluster-a").unwrap();
        let status = backend.status.clone();
        assert!(backend.read(backend.len().unwrap(), &mut [0; 1]).is_err());
        assert!(backend.write(0, &[0]).is_err());
        assert!(backend.set_len(0).is_err());
        assert!(backend.sync_data().is_err());
        assert!(backend.close().is_err());
        drop(backend);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut store = OwnedVoteStore::open(&path, "cluster-a").unwrap();
        // Feed the status produced by an actual backend error into the adapter;
        // even a rejection requiring no write must not serve failed authority.
        store.status = status;
        let mut gate = DurableVoteGate::new(1, store);
        let node = NodeId::from(1);
        let membership = Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
        assert_eq!(
            gate.request(
                node,
                None,
                &membership,
                RequestVote {
                    term: Term(0),
                    candidate: NodeId::from(2),
                    last_log: None
                }
            ),
            Err(VoteGateError::StorageFailure)
        );
    }

    #[test]
    fn waiting_operation_cannot_pass_a_failed_operation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        OwnedVoteStore::create(&path, "cluster-a")
            .unwrap()
            .close()
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let backend = OwnedBackend::open(&path, "cluster-a").unwrap();
        let (started, wait) = std::sync::mpsc::sync_channel(0);
        std::thread::scope(|scope| {
            let failing = scope.spawn(|| {
                backend.operate(|file| {
                    started.send(()).unwrap();
                    file.seek(SeekFrom::End(0))?;
                    file.read_exact(&mut [0; 1])
                })
            });
            wait.recv().unwrap();
            assert!(backend.write(0, &[0]).is_err());
            assert!(failing.join().unwrap().is_err());
        });
        drop(backend);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[derive(Debug)]
    struct FailingBackend {
        inner: OwnedBackend,
        armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fail_sync: bool,
    }

    impl StorageBackend for FailingBackend {
        fn len(&self) -> io::Result<u64> {
            self.inner.len()
        }
        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            self.inner.read(offset, out)
        }
        fn set_len(&self, len: u64) -> io::Result<()> {
            self.inner.set_len(len)
        }
        fn sync_data(&self) -> io::Result<()> {
            if self.fail_sync && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return self
                    .inner
                    .operate(|_| Err(io::Error::other("injected commit sync failure")));
            }
            self.inner.sync_data()
        }
        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            if !self.fail_sync && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return self.inner.operate(|file| {
                    file.seek(SeekFrom::Start(payload_offset(offset)?))?;
                    file.write_all(&data[..data.len() / 2])?;
                    Err(io::Error::other("injected partial commit write"))
                });
            }
            self.inner.write(offset, data)
        }
        fn close(&self) -> io::Result<()> {
            self.inner.close()
        }
    }

    #[test]
    fn failed_redb_commits_withhold_votes_and_recover_valid_authority() {
        for fail_sync in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("vote.redb");
            let node = NodeId::from(1);
            let original = HardState {
                term: Term(2),
                voted_for: Some(node),
            };
            let mut store = OwnedVoteStore::create(&path, "cluster-a").unwrap();
            store.persist(original).unwrap();
            store.close().unwrap();
            let inner = OwnedBackend::open(&path, "cluster-a").unwrap();
            let status = inner.status.clone();
            let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let database = Database::builder()
                .create_with_backend(FailingBackend {
                    inner,
                    armed: armed.clone(),
                    fail_sync,
                })
                .unwrap();
            let mut gate = DurableVoteGate::new(
                7,
                OwnedVoteStore {
                    database,
                    status,
                    state: original,
                    failed: false,
                },
            );
            let membership =
                Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
            let request = RequestVote {
                term: Term(3),
                candidate: node,
                last_log: None,
            };
            armed.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                gate.request(node, None, &membership, request),
                Err(VoteGateError::StorageFailure)
            );
            assert!(
                !armed.load(std::sync::atomic::Ordering::SeqCst),
                "fault was not exercised"
            );
            assert_eq!(
                gate.store.state(),
                original,
                "failed commit advanced cached authority"
            );
            assert_eq!(
                gate.request(node, None, &membership, request),
                Err(VoteGateError::StorageFailure)
            );
            assert!(gate.store.close().is_err());
            let recovered = OwnedVoteStore::open(&path, "cluster-a").unwrap();
            // A failed sync may have committed despite returning an error. Only
            // the old or requested complete authority is a valid recovery result.
            assert!(
                [
                    original,
                    HardState {
                        term: Term(3),
                        voted_for: Some(node)
                    }
                ]
                .contains(&recovered.state())
            );
            recovered.close().unwrap();
        }
    }

    #[test]
    fn initialization_crash_child() {
        let Some(path) = std::env::var_os("BMUX_OWNED_INIT_PATH") else {
            return;
        };
        if std::env::var_os("BMUX_OWNED_INIT_BOOTSTRAP").is_some() {
            let founder = NodeId::from(1);
            OwnedVoteStore::bootstrap(
                Path::new(&path),
                "cluster-a",
                founder,
                Membership::new(vec![vec![founder]], vec![(founder, String::new())]).unwrap(),
            )
            .unwrap();
        } else {
            OwnedVoteStore::create(Path::new(&path), "cluster-a").unwrap();
        }
        panic!("initialization crash hook was not exercised");
    }

    #[test]
    fn bootstrap_publication_crash_preserves_complete_authority() {
        for point in ["before-publish", "after-publish"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("authority.redb");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "owned_vote_store::tests::initialization_crash_child",
                    "--nocapture",
                ])
                .env("BMUX_OWNED_INIT_PATH", &path)
                .env("BMUX_OWNED_INIT_CRASH", point)
                .env("BMUX_OWNED_INIT_BOOTSTRAP", "1")
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(73));
            if point == "before-publish" {
                assert!(!path.exists());
                continue;
            }
            let mut store = OwnedVoteStore::open(&path, "cluster-a").unwrap();
            let founder = NodeId::from(1);
            let identity = LogIdentity {
                term: Term(0),
                leader: founder,
                index: crate::consensus_model::LogIndex(0),
            };
            assert!(
                store
                    .vote(
                        founder,
                        RequestVote {
                            term: Term(1),
                            candidate: founder,
                            last_log: Some(identity)
                        }
                    )
                    .unwrap()
                    .granted
            );
            store.close().unwrap();
        }
    }

    #[test]
    fn publication_interruption_recovers_only_canonical_authority() {
        for point in ["before-publish", "after-publish"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("vote.redb");
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "owned_vote_store::tests::initialization_crash_child",
                    "--nocapture",
                ])
                .env("BMUX_OWNED_INIT_PATH", &path)
                .env("BMUX_OWNED_INIT_CRASH", point)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(73));
            let store = if point == "before-publish" {
                assert!(!path.exists());
                // Orphans are never adopted as authority; explicit initialization
                // can publish a fresh canonical inode without reading them.
                OwnedVoteStore::create(&path, "cluster-a").unwrap()
            } else {
                assert!(OwnedVoteStore::create(&path, "cluster-a").is_err());
                OwnedVoteStore::open(&path, "cluster-a").unwrap()
            };
            assert_eq!(
                store.state(),
                HardState {
                    term: Term(0),
                    voted_for: None
                }
            );
            store.close().unwrap();
        }
    }

    #[test]
    fn unsupported_payload_is_rejected_without_mutating_authority() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let store = OwnedVoteStore::create(&path, "cluster-a").unwrap();
        let tx = store.database.begin_write().unwrap();
        tx.open_table(TABLE)
            .unwrap()
            .insert("format", b"future-format".as_slice())
            .unwrap();
        tx.commit().unwrap();
        store.close().unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(OwnedVoteStore::open(&path, "cluster-a").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn backend_rejects_growth_before_changing_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        OwnedVoteStore::create(&path, "cluster-a")
            .unwrap()
            .close()
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        for resize in [false, true] {
            let backend = OwnedBackend::open(&path, "cluster-a").unwrap();
            let result = if resize {
                backend.set_len(MAX_VOTE_PAYLOAD + 1)
            } else {
                backend.write(MAX_VOTE_PAYLOAD, &[1])
            };
            assert!(result.is_err());
            drop(backend);
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        assert!(check_payload_range(MAX_VOTE_PAYLOAD, 0).is_ok());
        assert!(check_payload_range(u64::MAX, 1).is_err());
    }

    #[test]
    fn preflight_admission_is_bounded_and_released() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        OwnedVoteStore::create(&path, "cluster-a")
            .unwrap()
            .close()
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let admission = admit_preflight().unwrap();
        let error = OwnedVoteStore::open(&path, "cluster-a").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(admission);
        OwnedVoteStore::open(&path, "cluster-a")
            .unwrap()
            .close()
            .unwrap();
    }

    // Deliberately hold admission to prove recovery uses try_lock rather than
    // blocking the worker; this test would expose a blocking-admission regression.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn executor_recovers_authority_and_reports_preflight_backpressure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        let mut store = OwnedVoteStore::create(&path, "cluster-a").unwrap();
        store
            .persist(HardState {
                term: Term(4),
                voted_for: Some(NodeId::from(1)),
            })
            .unwrap();
        store.close().unwrap();
        let admission = admit_preflight().unwrap();
        assert_eq!(
            VoteExecutor::recover(path.clone(), "cluster-a".into(), 1)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(admission);
        let executor = VoteExecutor::recover(path.clone(), "cluster-a".into(), 2)
            .await
            .unwrap();
        let node = NodeId::from(1);
        let membership = Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
        let response = executor
            .request_with_context(
                node,
                None,
                membership,
                RequestVote {
                    term: Term(3),
                    candidate: node,
                    last_log: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(response.term, Term(4));
        assert!(!response.granted);
        executor.shutdown().await.unwrap();
        OwnedVoteStore::open(&path, "cluster-a")
            .unwrap()
            .close()
            .unwrap();
    }

    #[test]
    fn cancelled_recovery_handoff_releases_authority() {
        for cancel_before_delivery in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("vote.redb");
            let mut store = OwnedVoteStore::create(&path, "cluster-a").unwrap();
            let expected = HardState {
                term: Term(4),
                voted_for: Some(NodeId::from(1)),
            };
            store.persist(expected).unwrap();
            let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
            let (send, receive) = tokio::sync::oneshot::channel();
            let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
            if cancel_before_delivery {
                drop(receive);
                drop(acknowledge);
                executor.handoff(send, acknowledged).unwrap();
            } else {
                let worker = std::thread::spawn(move || executor.handoff(send, acknowledged));
                let delivered = receive.blocking_recv().unwrap().unwrap();
                drop(acknowledge);
                worker.join().unwrap().unwrap();
                // Even a retained receiver-side handle cannot retain authority
                // after rejecting the handoff.
                assert!(delivered.gate.blocking_lock().is_none());
            }
            let reopened = OwnedVoteStore::open(&path, "cluster-a").unwrap();
            assert_eq!(reopened.state(), expected);
            reopened.close().unwrap();
        }
    }

    #[test]
    fn incomplete_metadata_is_not_initialized_on_open() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("vote.redb");
        drop(Database::create(&path).unwrap());
        assert!(OwnedVoteStore::open(&path, "cluster-a").is_err());
        assert!(OwnedVoteStore::create(&path, "cluster-a").is_err());
        assert!(decode(&[0; 40]).is_err());
    }
}
